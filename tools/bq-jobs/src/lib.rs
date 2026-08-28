//! Query BigQuery's own job history: what ran, what it cost, what was slow.
//!
//! ## Why a tool rather than "just write the SQL"
//!
//! `INFORMATION_SCHEMA.JOBS` answers the questions that matter about spend —
//! which queries cost the most, who ran them, what is slow — but it is
//! genuinely awkward to query by hand:
//!
//! * It **must** carry a region qualifier, and the syntax is the unusual
//!   `` `region-us`.INFORMATION_SCHEMA.JOBS ``. Omitting it is an error, and
//!   getting it wrong silently returns another region's jobs.
//! * `creation_time` is the partition column, so a query without a filter on it
//!   scans all retained history.
//! * The interesting numbers are buried: bytes are in `total_bytes_billed`,
//!   duration must be computed from two timestamps, and slot usage is in
//!   `total_slot_ms`.
//!
//! So this tool composes the SQL from a few plain arguments, always partitions
//! the scan, and converts the raw figures into money and seconds.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_f64_loose, as_u64_loose, bool_arg, clip, commas, decode_rows, estimate_cost, human_bytes,
    parse_schema, render_cost, str_arg, u64_arg, Bq,
};
use serde_json::{json, Value};

const DEFAULT_DAYS: u64 = 7;
const DEFAULT_LIMIT: u64 = 20;
const POLL_TIMEOUT_MS: u64 = 30_000;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-jobs".to_string(),
            description: "Inspect BigQuery job history from INFORMATION_SCHEMA.JOBS: which \
                queries ran, who ran them, how many bytes each scanned, what it cost, how \
                long it took and how many slots it used. Use it to find the expensive or \
                slow queries in a project, or to audit recent activity. Sorts by cost by \
                default. Note this is itself a query and is billed at a 10 MB minimum, \
                unlike the metadata tools. Reaches bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "days": {
                        "type": "integer",
                        "description": "How many days back to look. Default 7. This bounds the partition scan, so a smaller number is cheaper."
                    },
                    "order_by": {
                        "type": "string",
                        "enum": ["cost", "duration", "slots", "recent"],
                        "description": "What counts as interesting. Default 'cost'."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Rows to return. Default 20."
                    },
                    "user": {
                        "type": "string",
                        "description": "Only jobs run by this principal, matched as a substring of the email."
                    },
                    "state": {
                        "type": "string",
                        "enum": ["DONE", "RUNNING", "PENDING"],
                        "description": "Only jobs in this state. Omit for all."
                    },
                    "errors_only": {
                        "type": "boolean",
                        "description": "Only jobs that failed. Good for finding a broken scheduled query."
                    },
                    "min_gib": {
                        "type": "number",
                        "description": "Only jobs that scanned at least this many GiB, to filter out trivia."
                    },
                    "region": {
                        "type": "string",
                        "description": "Region qualifier, e.g. 'us', 'eu', 'europe-west2'. Defaults to the configured location. Jobs are only visible in the region they ran in."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project. Defaults to the configured project."
                    },
                    "show_sql": {
                        "type": "boolean",
                        "description": "Include a snippet of each query's text. Default true."
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

        let region = str_arg(&args, "region")
            .or_else(|| client.location.clone())
            .unwrap_or_else(|| "us".to_string())
            .to_lowercase();
        let region = region.strip_prefix("region-").unwrap_or(&region).to_string();
        validate_region(&region)?;

        let days = u64_arg(&args, "days").unwrap_or(DEFAULT_DAYS).clamp(1, 180);
        let limit = u64_arg(&args, "limit").unwrap_or(DEFAULT_LIMIT).clamp(1, 500);

        // creation_time is the partition column: filtering on it is what keeps
        // this from scanning all retained history.
        let mut predicates = vec![format!(
            "creation_time >= TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL {days} DAY)"
        )];
        let mut parameters = json!({});

        if let Some(user) = str_arg(&args, "user") {
            predicates.push("user_email LIKE CONCAT('%', @user, '%')".to_string());
            parameters["user"] = json!(user);
        }
        if let Some(state) = str_arg(&args, "state") {
            let state = state.to_uppercase();
            if !["DONE", "RUNNING", "PENDING"].contains(&state.as_str()) {
                return Err(format!("state {state:?} is not one of DONE, RUNNING, PENDING"));
            }
            predicates.push("state = @state".to_string());
            parameters["state"] = json!(state);
        }
        if bool_arg(&args, "errors_only").unwrap_or(false) {
            predicates.push("error_result IS NOT NULL".to_string());
        }
        if let Some(gib) = args.get("min_gib").and_then(as_f64_loose) {
            let bytes = (gib * 1_073_741_824.0) as u64;
            predicates.push(format!("total_bytes_billed >= {bytes}"));
        }

        let order = match str_arg(&args, "order_by").as_deref() {
            Some("duration") => "duration_ms DESC",
            Some("slots") => "total_slot_ms DESC",
            Some("recent") => "creation_time DESC",
            // Cost is the default because it is the question people actually
            // have, and NULLS LAST keeps unbilled jobs out of the top.
            _ => "total_bytes_billed DESC NULLS LAST",
        };

        let sql = format!(
            "SELECT
               job_id,
               user_email,
               job_type,
               statement_type,
               state,
               creation_time,
               TIMESTAMP_DIFF(end_time, start_time, MILLISECOND) AS duration_ms,
               total_bytes_processed,
               total_bytes_billed,
               total_slot_ms,
               cache_hit,
               error_result.message AS error_message,
               query
             FROM `{project}`.`region-{region}`.INFORMATION_SCHEMA.JOBS
             WHERE {}
             ORDER BY {order}
             LIMIT {limit}",
            predicates.join("\n               AND ")
        );

        let mut request = json!({
            "query": sql,
            "useLegacySql": false,
            "maxResults": limit,
            "timeoutMs": POLL_TIMEOUT_MS,
            // INFORMATION_SCHEMA results are never cached anyway.
            "useQueryCache": false,
        });
        if let Some(map) = parameters.as_object().filter(|m| !m.is_empty()) {
            let bound: Vec<Value> = map
                .iter()
                .map(|(name, value)| {
                    json!({
                        "name": name,
                        "parameterType": { "type": "STRING" },
                        "parameterValue": { "value": value },
                    })
                })
                .collect();
            request["parameterMode"] = json!("NAMED");
            request["queryParameters"] = json!(bound);
        }
        // The JOBS view is regional, and the job must run in that region.
        request["location"] = json!(region.clone());

        let response = client
            .post(&format!("/projects/{project}/queries"), &request)
            .map_err(|e| {
                if e.contains("not found") || e.contains("Not found") {
                    format!(
                        "{e}\n\nJobs are only visible in the region they ran in. This looked \
                         in region-{region}; pass `region` to look elsewhere."
                    )
                } else {
                    e
                }
            })?;

        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let raw_rows = response
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rows = decode_rows(&fields, &raw_rows);

        if rows.is_empty() {
            return Ok(format!(
                "no jobs in {project} region-{region} in the last {days} day(s) matching \
                 those filters.\n\nIf you expected some: jobs are only visible in the \
                 region they ran in, and only to a principal with \
                 roles/bigquery.resourceViewer for other users' jobs."
            ));
        }

        let show_sql = bool_arg(&args, "show_sql").unwrap_or(true);
        let mut out = String::new();
        out.push_str(&format!(
            "{} job(s) in {project} region-{region}, last {days} day(s), by {}:\n\n",
            rows.len(),
            str_arg(&args, "order_by").unwrap_or_else(|| "cost".to_string())
        ));

        let mut total_billed = 0u64;
        for row in &rows {
            let get = |key: &str| row.get(key).cloned().unwrap_or(Value::Null);
            let billed = as_u64_loose(&get("total_bytes_billed")).unwrap_or(0);
            let processed = as_u64_loose(&get("total_bytes_processed")).unwrap_or(0);
            total_billed += billed;

            let job_id = get("job_id").as_str().unwrap_or("?").to_string();
            let user = get("user_email").as_str().unwrap_or("?").to_string();
            let state = get("state").as_str().unwrap_or("?").to_string();
            let statement = get("statement_type")
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| get("job_type").as_str().unwrap_or("").to_string());
            let cached = get("cache_hit").as_bool().unwrap_or(false);

            out.push_str(&format!("  {job_id}\n"));
            out.push_str(&format!(
                "    {user}  ·  {state}{}{}\n",
                if statement.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {statement}")
                },
                if cached { "  ·  cached" } else { "" }
            ));

            let duration = as_u64_loose(&get("duration_ms"));
            let slots = as_u64_loose(&get("total_slot_ms"));
            let mut stats = Vec::new();
            if billed > 0 || processed > 0 {
                stats.push(format!(
                    "scanned {} · {}",
                    human_bytes(processed.max(billed)),
                    render_cost(estimate_cost(billed, client.price_per_tib))
                ));
            } else if cached {
                stats.push("free (cache hit)".to_string());
            }
            if let Some(ms) = duration {
                stats.push(format!("{:.1}s", ms as f64 / 1000.0));
            }
            if let Some(ms) = slots {
                stats.push(format!("{} slot-s", commas(ms / 1000)));
            }
            if !stats.is_empty() {
                out.push_str(&format!("    {}\n", stats.join("  ·  ")));
            }

            if let Some(error) = get("error_message").as_str().filter(|e| !e.is_empty()) {
                out.push_str(&format!("    ERROR: {}\n", clip(error, 200)));
            }
            if show_sql {
                if let Some(query) = get("query").as_str().filter(|q| !q.is_empty()) {
                    // Collapse to one line: job SQL is often heavily formatted
                    // and a 40-line query per row buries the numbers.
                    let flat = query.split_whitespace().collect::<Vec<_>>().join(" ");
                    out.push_str(&format!("    {}\n", clip(&flat, 200)));
                }
            }
            out.push('\n');
        }

        out.push_str(&format!(
            "total billed across these {} job(s): {} — {}\n",
            rows.len(),
            human_bytes(total_billed),
            render_cost(estimate_cost(total_billed, client.price_per_tib))
        ));

        if let Some(scanned) = response.get("totalBytesProcessed").and_then(as_u64_loose) {
            out.push_str(&format!(
                "(this lookup itself scanned {} and is billed at a 10 MB minimum)\n",
                human_bytes(scanned)
            ));
        }

        Ok(out)
    }
}

/// Region qualifiers are interpolated into SQL, so they cannot be free text.
fn validate_region(region: &str) -> Result<(), String> {
    if region.is_empty() {
        return Err("region must not be empty".to_string());
    }
    if !region
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(format!(
            "region {region:?} is not a valid qualifier. Expected something like 'us', \
             'eu' or 'europe-west2'."
        ));
    }
    Ok(())
}

export!(Component);
