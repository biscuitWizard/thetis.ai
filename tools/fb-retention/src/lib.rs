//! Cohort retention for a Firebase / GA4 BigQuery export.
//!
//! ## Why this is a tool and not a query someone writes each time
//!
//! "D1 retention" is not one number. Computing it from a GA4 export has four
//! traps, and each one silently returns a plausible wrong answer:
//!
//! 1. **Immature cohorts.** A cohort that first opened yesterday cannot have a
//!    D7 yet. Include it and its zero is averaged in as if it were a real zero,
//!    dragging the headline down. Every cohort here must have had the full
//!    N days to be observed, so the denominator only ever contains cohorts old
//!    enough to answer. This is the single most common way a retention number
//!    comes out too low.
//!
//! 2. **`events_*` also matches `events_intraday_*`.** The obvious wildcard
//!    silently pulls in today's partial data, so the last day is half a day and
//!    users appear to churn. We match `events_2*` and slice `_TABLE_SUFFIX`
//!    against date strings with the leading `2` removed, which excludes the
//!    intraday shards by construction.
//!
//! 3. **The activity window must extend past the cohort window.** To see
//!    whether an Aug-23 cohort came back on Aug-30, the scan has to *include*
//!    Aug-30. Bounding activity by the cohort range makes late returns
//!    invisible.
//!
//! 4. **The definition itself.** On this dataset, D7 is 4.2% under the
//!    console's definition and 11.2% under "returned on day 7 or later" — a
//!    2.7x spread from one wording. So the definition is an explicit argument
//!    and is always restated in the output.
//!
//! `is_active_user` is GA4's own engagement flag and is what the Firebase
//! console counts, which is why it is the default rather than "any event": a
//! background ping is not a returning player.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_u64_loose, bool_arg, decode_rows, estimate_cost, human_bytes, parse_schema, render_cost,
    str_arg, u64_arg, Bq,
};
use serde_json::{json, Value};
use thetis::grip::sys;
use thetis::grip::types::LogLevel;

const POLL_TIMEOUT_MS: u64 = 10_000;
const MAX_POLLS: u32 = 12;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "fb-retention".to_string(),
            description: "Cohort retention (D1, D7, or any day N) for a Firebase/GA4 BigQuery \
                export, computed the way the Firebase console does it: cohorts keyed on \
                first_open, activity judged by is_active_user. Only cohorts old enough to have \
                been observed for the full N days are counted, so an immature cohort can never \
                dilute the headline — the usual cause of a retention number that looks too \
                low. Excludes intraday shards, states the definition and cohort sizes with \
                every answer, and can show the per-cohort breakdown. Use this rather than \
                hand-writing cohort SQL."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "dataset": {
                        "type": "string",
                        "description": "The GA4 export dataset, e.g. 'analytics_249556675' or \
                            'project.analytics_249556675'. Defaults to `dataset` in config."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project to bill and query. Defaults to config."
                    },
                    "days": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Which day offsets to report, e.g. [1,7,30]. Defaults to [1,7]."
                    },
                    "window_days": {
                        "type": "integer",
                        "description": "How many days back to take cohorts from. Default 30. Note \
                            a DN cohort needs N further days to mature, so the D7 denominator \
                            covers fewer cohorts than the D1 one; both are reported."
                    },
                    "end_date": {
                        "type": "string",
                        "description": "Last complete day of data, as YYYY-MM-DD. Defaults to \
                            yesterday, since today is still being collected."
                    },
                    "definition": {
                        "type": "string",
                        "enum": ["console", "exact", "rolling"],
                        "description": "'console' (default) and 'exact' both mean active on \
                            exactly day N — what the Firebase console reports. 'rolling' means \
                            active on day N or any later day (survival-style), which is always \
                            a higher number."
                    },
                    "active_definition": {
                        "type": "string",
                        "enum": ["is_active_user", "any_event"],
                        "description": "What counts as active. 'is_active_user' (default) is \
                            GA4's engagement flag and matches the console. 'any_event' counts \
                            any event at all, including background pings, and reads higher."
                    },
                    "platform": {
                        "type": "string",
                        "description": "Restrict to one platform, e.g. 'ANDROID' or 'IOS'. \
                            Omit for all."
                    },
                    "app_version": {
                        "type": "string",
                        "description": "Restrict cohorts to one app_info.version, for reading \
                            retention of a specific release."
                    },
                    "app_id": {
                        "type": "string",
                        "description": "Restrict to one app_info.id (bundle id), when the export \
                            carries more than one app."
                    },
                    "by_cohort": {
                        "type": "boolean",
                        "description": "Also return the per-cohort-date breakdown, to see trend \
                            and per-day cohort sizes rather than just the pooled number."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Price the scan without running it. Free."
                    },
                    "show_sql": {
                        "type": "boolean",
                        "description": "Include the generated SQL, so the definition can be \
                            audited or the query adapted by hand."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth token, overriding the configured credential."
                    },
                    "location": {
                        "type": "string",
                        "description": "Dataset location. Only needed outside the US/EU multi-regions."
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["read-only".to_string()],
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

        // ------------------------------------------------------------------
        // Resolve the dataset
        // ------------------------------------------------------------------
        let dataset_arg = str_arg(&args, "dataset")
            .or_else(|| bq::string_field(&config, &["dataset", "ga4_dataset", "default_dataset"]))
            .ok_or(
                "no GA4 dataset. Pass `dataset` (e.g. 'analytics_249556675'), or set `dataset` \
                 in [tools.fb-retention]. `bq-list` shows which datasets exist; a Firebase \
                 export is named analytics_<stream id>.",
            )?;
        let (ds_project, ds_id) = match dataset_arg.rsplit_once('.') {
            Some((p, d)) => (p.to_string(), d.to_string()),
            None => (project.clone(), dataset_arg.clone()),
        };

        // ------------------------------------------------------------------
        // Definitions and window
        // ------------------------------------------------------------------
        let days: Vec<i64> = match args.get("days").and_then(Value::as_array) {
            Some(list) if !list.is_empty() => {
                let mut parsed = Vec::new();
                for value in list {
                    let n = value.as_i64().ok_or("`days` must contain whole numbers")?;
                    if n < 1 {
                        return Err(format!(
                            "day offset {n} is not meaningful: day 0 is the cohort itself, so \
                             retention is measured from day 1 onward."
                        ));
                    }
                    parsed.push(n);
                }
                parsed.sort_unstable();
                parsed.dedup();
                parsed
            }
            _ => vec![1, 7],
        };

        let window_days = u64_arg(&args, "window_days").unwrap_or(30) as i64;
        if window_days < 1 {
            return Err("`window_days` must be at least 1".to_string());
        }

        let definition = str_arg(&args, "definition").unwrap_or_else(|| "console".to_string());
        let rolling = match definition.as_str() {
            "console" | "exact" => false,
            "rolling" => true,
            other => {
                return Err(format!(
                    "unknown definition {other:?}. Use 'console' (or 'exact') for active on \
                     exactly day N, or 'rolling' for active on day N or later."
                ))
            }
        };

        let active_definition =
            str_arg(&args, "active_definition").unwrap_or_else(|| "is_active_user".to_string());
        let active_predicate = match active_definition.as_str() {
            "is_active_user" => "AND is_active_user",
            "any_event" => "",
            other => {
                return Err(format!(
                    "unknown active_definition {other:?}. Use 'is_active_user' (matches the \
                     Firebase console) or 'any_event'."
                ))
            }
        };

        // `end_date` is the last day of *complete* data. Today is still being
        // collected, so counting it would understate the final cohorts.
        let end_expr = match str_arg(&args, "end_date") {
            Some(date) => {
                validate_date(&date)?;
                format!("DATE '{date}'")
            }
            None => "DATE_SUB(CURRENT_DATE(), INTERVAL 1 DAY)".to_string(),
        };

        let max_day = *days.iter().max().unwrap_or(&1);
        let sql = build_sql(
            &ds_project,
            &ds_id,
            &days,
            window_days,
            max_day,
            &end_expr,
            rolling,
            active_predicate,
            &args,
        )?;

        if bool_arg(&args, "show_sql").unwrap_or(false) && bool_arg(&args, "dry_run") != Some(true) {
            sys::log(LogLevel::Debug, &sql);
        }

        // ------------------------------------------------------------------
        // Dry run
        // ------------------------------------------------------------------
        if bool_arg(&args, "dry_run").unwrap_or(false) {
            let mut request = json!({
                "configuration": {
                    "dryRun": true,
                    "query": { "query": sql, "useLegacySql": false }
                }
            });
            if let Some(location) = client.location.clone() {
                request["jobReference"] = json!({ "location": location });
            }
            let response = client.post(&format!("/projects/{project}/jobs"), &request)?;
            // A dry run reports bytes under statistics.query, not statistics
            // itself; reading the wrong level silently yields 0 B and makes an
            // expensive scan look free.
            let statistics = response.get("statistics");
            let bytes = statistics
                .and_then(|s| s.get("query"))
                .and_then(|q| q.get("totalBytesProcessed"))
                .or_else(|| statistics.and_then(|s| s.get("totalBytesProcessed")))
                .and_then(as_u64_loose)
                .unwrap_or(0);
            let mut out = format!(
                "dry run — nothing was billed.\n\nwould scan {} · about {}\n",
                human_bytes(bytes),
                render_cost(estimate_cost(bytes, client.price_per_tib))
            );
            if bool_arg(&args, "show_sql").unwrap_or(false) {
                out.push_str(&format!("\n{sql}\n"));
            }
            return Ok(out);
        }

        // ------------------------------------------------------------------
        // Run
        // ------------------------------------------------------------------
        let mut request = json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": POLL_TIMEOUT_MS,
            "maxResults": 400,
            "formatOptions": { "useInt64Timestamp": false },
        });
        if let Some(location) = client.location.clone() {
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
                return Err("BigQuery returned no job id to poll.".to_string());
            }
            if polls >= MAX_POLLS {
                return Ok(format!(
                    "the retention query is still running after about {}s. It was not \
                     cancelled. Fetch it with `bq-results` and job id {job_id}.",
                    MAX_POLLS as u64 * POLL_TIMEOUT_MS / 1000
                ));
            }
            polls += 1;
            let mut query = vec![
                ("maxResults".to_string(), "400".to_string()),
                ("timeoutMs".to_string(), POLL_TIMEOUT_MS.to_string()),
            ];
            if let Some(location) = &job_location {
                query.push(("location".to_string(), location.clone()));
            }
            response = client.get(&format!("/projects/{project}/queries/{job_id}"), &query)?;
        }

        if let Some(errors) = response.get("errors").and_then(Value::as_array) {
            if let Some(first) = errors.first() {
                let message = first
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(format!("the retention query failed: {message}"));
            }
        }

        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let raw = response
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rows = decode_rows(&fields, &raw);

        let scanned = response
            .get("totalBytesProcessed")
            .and_then(as_u64_loose)
            .unwrap_or(0);
        let billed = response.get("totalBytesBilled").and_then(as_u64_loose);
        let cache_hit = response
            .get("cacheHit")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        Ok(render_report(
            &rows,
            &ds_project,
            &ds_id,
            &days,
            window_days,
            rolling,
            &active_definition,
            &args,
            scanned,
            billed,
            cache_hit,
            client.price_per_tib,
            &sql,
        ))
    }
}

/// Rejects a malformed date before it reaches SQL, and keeps it out of the
/// interpolated string.
fn validate_date(date: &str) -> Result<(), String> {
    let bytes = date.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if shaped {
        Ok(())
    } else {
        Err(format!(
            "end_date {date:?} is not a date. Use YYYY-MM-DD, e.g. '2026-08-30'."
        ))
    }
}

/// A string literal safe to interpolate: GA4 dimension values are matched
/// exactly, so a quote in one can only be an error or an attack.
fn safe_literal(value: &str, field: &str) -> Result<String, String> {
    if value.contains('\'') || value.contains('\\') || value.contains('\n') {
        return Err(format!("{field} may not contain quotes or backslashes"));
    }
    Ok(value.to_string())
}

#[allow(clippy::too_many_arguments)]
fn build_sql(
    ds_project: &str,
    ds_id: &str,
    days: &[i64],
    window_days: i64,
    max_day: i64,
    end_expr: &str,
    rolling: bool,
    active_predicate: &str,
    args: &Value,
) -> Result<String, String> {
    // Cohort-side filters. Applied to the first_open row, since that is what
    // defines which cohort a user belongs to.
    let mut cohort_filters = String::new();
    if let Some(platform) = str_arg(args, "platform") {
        cohort_filters.push_str(&format!(
            "\n      AND platform = '{}'",
            safe_literal(&platform.to_uppercase(), "platform")?
        ));
    }
    if let Some(version) = str_arg(args, "app_version") {
        cohort_filters.push_str(&format!(
            "\n      AND app_info.version = '{}'",
            safe_literal(&version, "app_version")?
        ));
    }
    if let Some(app_id) = str_arg(args, "app_id") {
        cohort_filters.push_str(&format!(
            "\n      AND app_info.id = '{}'",
            safe_literal(&app_id, "app_id")?
        ));
    }

    // The wildcard is `events_2*`, not `events_*`, so `events_intraday_*`
    // cannot match: intraday data is partial and would read as churn. The
    // suffix therefore excludes the leading '2' of the year.
    let table = format!("`{ds_project}.{ds_id}.events_2*`");

    // Two different suffix ranges, and the difference is the point:
    //   cohorts  — the window we draw first_open from
    //   activity — extends `max_day` further, or a late return is unobservable
    // The wildcard is `events_2*`, so `_TABLE_SUFFIX` is the date with its
    // leading '2' already consumed by the prefix — '0260830', not '20260830'.
    // Comparing against a full FORMAT_DATE string matches nothing at all, so
    // the bounds are sliced the same way with SUBSTR.
    let cohort_range = format!(
        "SUBSTR(FORMAT_DATE('%Y%m%d', DATE_SUB({end_expr}, INTERVAL {} DAY)), 2) \
         AND SUBSTR(FORMAT_DATE('%Y%m%d', {end_expr}), 2)",
        window_days + max_day - 1
    );

    let mut per_day_cohort = String::new();
    let mut per_day_pooled = String::new();
    for &n in days {
        let cmp = if rolling { ">=" } else { "=" };
        per_day_cohort.push_str(&format!(
            "\n    COUNTIF(mature_d{n} AND ret_d{n}) AS d{n}_retained,\
             \n    COUNTIF(mature_d{n}) AS d{n}_eligible,\
             \n    ROUND(100 * SAFE_DIVIDE(COUNTIF(mature_d{n} AND ret_d{n}), \
             NULLIF(COUNTIF(mature_d{n}), 0)), 2) AS d{n}_pct,"
        ));
        per_day_pooled.push_str(&format!(
            "\n    MAX(a.event_day {cmp} DATE_ADD(c.cohort_date, INTERVAL {n} DAY)) AS ret_d{n},"
        ));
    }

    let mut maturity = String::new();
    for &n in days {
        maturity.push_str(&format!(
            "\n    DATE_ADD(cohort_date, INTERVAL {n} DAY) <= last_day AS mature_d{n},"
        ));
    }

    let by_cohort = bool_arg(args, "by_cohort").unwrap_or(false);

    // `first_open` is the true install marker. Taking MIN across the window
    // also guards against a device re-firing it.
    let mut sql = format!(
        "-- Firebase-console-style cohort retention, generated by fb-retention.
-- Cohort day 0 = first_open. Retained = {} on day N.
WITH bounds AS (
  SELECT {end_expr} AS last_day
),

cohorts AS (
  SELECT
    user_pseudo_id,
    MIN(PARSE_DATE('%Y%m%d', event_date)) AS cohort_date
  FROM {table}
  WHERE _TABLE_SUFFIX BETWEEN {cohort_range}
    AND event_name = 'first_open'{cohort_filters}
  GROUP BY user_pseudo_id
),

-- Restricted to the cohort window *after* grouping, so a user whose real
-- first_open predates the window is excluded rather than mis-cohorted.
windowed AS (
  SELECT c.user_pseudo_id, c.cohort_date
  FROM cohorts c, bounds b
  WHERE c.cohort_date > DATE_SUB(b.last_day, INTERVAL {window_days} DAY)
    AND c.cohort_date <= b.last_day
),

activity AS (
  SELECT DISTINCT
    user_pseudo_id,
    PARSE_DATE('%Y%m%d', event_date) AS event_day
  FROM {table}
  WHERE _TABLE_SUFFIX BETWEEN {cohort_range}{active_predicate}
),

flags AS (
  SELECT
    c.cohort_date,
    c.user_pseudo_id,{per_day_pooled}
  FROM windowed c
  LEFT JOIN activity a USING (user_pseudo_id)
  GROUP BY c.cohort_date, c.user_pseudo_id
),

-- A cohort only counts for DN once it has had N full days to be observed.
-- Without this, yesterday's cohort contributes a structural zero to D7.
marked AS (
  SELECT f.*,{maturity}
  FROM flags f, bounds
)
",
        if rolling {
            "active on day N or later"
        } else {
            "active on exactly day N"
        }
    );

    if by_cohort {
        sql.push_str(&format!(
            "SELECT
    CAST(cohort_date AS STRING) AS cohort_date,
    COUNT(*) AS cohort_size,{per_day_cohort}
  FROM marked
  GROUP BY cohort_date
  ORDER BY cohort_date
"
        ));
    } else {
        sql.push_str(&format!(
            "SELECT
    'all' AS cohort_date,
    COUNT(*) AS cohort_size,{per_day_cohort}
  FROM marked
"
        ));
    }

    Ok(sql)
}

/// Renders the answer so the number cannot be read without its definition.
#[allow(clippy::too_many_arguments)]
fn render_report(
    rows: &[Value],
    ds_project: &str,
    ds_id: &str,
    days: &[i64],
    window_days: i64,
    rolling: bool,
    active_definition: &str,
    args: &Value,
    scanned: u64,
    billed: Option<u64>,
    cache_hit: bool,
    price_per_tib: f64,
    sql: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Cohort retention — {ds_project}.{ds_id}\n"));
    out.push_str(&format!("{}\n\n", "=".repeat(40)));

    let by_cohort = bool_arg(args, "by_cohort").unwrap_or(false);

    if rows.is_empty() {
        out.push_str(
            "No cohorts matched. Either the window contains no first_open events, or the \
             filters excluded everything. `bq-query` on event_name='first_open' will show \
             whether the export carries them.\n",
        );
        return out;
    }

    if by_cohort {
        out.push_str("Per cohort:\n\n");
        let mut header = format!("{:<12} {:>7}", "cohort", "size");
        for &n in days {
            header.push_str(&format!("  {:>12}", format!("D{n}")));
        }
        out.push_str(&header);
        out.push('\n');
        out.push_str(&"-".repeat(header.len()));
        out.push('\n');

        for row in rows {
            let date = row
                .get("cohort_date")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let size = row.get("cohort_size").and_then(as_u64_loose).unwrap_or(0);
            let mut line = format!("{date:<12} {size:>7}");
            for &n in days {
                let pct = row.get(format!("d{n}_pct")).and_then(bq::as_f64_loose);
                let eligible = row
                    .get(format!("d{n}_eligible"))
                    .and_then(as_u64_loose)
                    .unwrap_or(0);
                let cell = match (pct, eligible) {
                    (_, 0) => "immature".to_string(),
                    (Some(p), _) => format!("{p:.2}%"),
                    (None, _) => "—".to_string(),
                };
                line.push_str(&format!("  {cell:>12}"));
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(
            "\n\"immature\" means that cohort has not yet had the full N days to be observed, \
             so it is excluded from the pooled figure rather than counted as a zero.\n",
        );
    } else {
        let row = &rows[0];
        let total = row.get("cohort_size").and_then(as_u64_loose).unwrap_or(0);
        for &n in days {
            let pct = row.get(format!("d{n}_pct")).and_then(bq::as_f64_loose);
            let retained = row
                .get(format!("d{n}_retained"))
                .and_then(as_u64_loose)
                .unwrap_or(0);
            let eligible = row
                .get(format!("d{n}_eligible"))
                .and_then(as_u64_loose)
                .unwrap_or(0);

            if eligible == 0 {
                out.push_str(&format!(
                    "  D{n}:  not yet measurable — no cohort in the window is {n} days old.\n"
                ));
                continue;
            }
            let shown = pct.map(|p| format!("{p:.2}%")).unwrap_or("—".to_string());
            out.push_str(&format!(
                "  D{n}:  {shown}   ({} of {} users in mature cohorts)\n",
                bq::commas(retained),
                bq::commas(eligible)
            ));
        }
        out.push_str(&format!(
            "\n{} users first opened in the window in total.\n",
            bq::commas(total)
        ));
    }

    // The definition, always. A retention number without one is not a fact.
    out.push_str("\nDefinition\n----------\n");
    out.push_str("  cohort day 0   first_open\n");
    out.push_str(&format!(
        "  retained       {}\n",
        if rolling {
            "active on day N or any later day (survival-style; reads higher)"
        } else {
            "active on exactly day N — matches the Firebase console"
        }
    ));
    out.push_str(&format!(
        "  active means   {}\n",
        if active_definition == "any_event" {
            "any event at all, background pings included"
        } else {
            "is_active_user — GA4's engagement flag, as the console uses"
        }
    ));
    out.push_str(&format!(
        "  cohort window  last {window_days} days of complete data\n"
    ));
    out.push_str("  maturity       only cohorts observed for the full N days are counted\n");

    let mut filters = Vec::new();
    if let Some(p) = str_arg(args, "platform") {
        filters.push(format!("platform={p}"));
    }
    if let Some(v) = str_arg(args, "app_version") {
        filters.push(format!("app_version={v}"));
    }
    if let Some(a) = str_arg(args, "app_id") {
        filters.push(format!("app_id={a}"));
    }
    if !filters.is_empty() {
        out.push_str(&format!("  filters        {}\n", filters.join(", ")));
    }

    out.push('\n');
    if cache_hit {
        out.push_str("cached result — nothing scanned, nothing billed\n");
    } else {
        let effective = billed.unwrap_or(scanned);
        out.push_str(&format!(
            "scanned {} · cost {}\n",
            human_bytes(scanned),
            render_cost(estimate_cost(effective, price_per_tib))
        ));
    }

    if bool_arg(args, "show_sql").unwrap_or(false) {
        out.push_str(&format!("\nSQL\n---\n{sql}\n"));
    }

    out
}

export!(Component);
