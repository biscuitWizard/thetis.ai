//! Re-read the results of a query job that has already run.
//!
//! ## Why paging deserves its own tool
//!
//! A query's results live in a temporary table for 24 hours, and reading them
//! again through `jobs.getQueryResults` is **free** — the scan was billed once,
//! when the job ran. Re-issuing the same SQL to see rows 200-400 would scan and
//! bill the whole thing a second time.
//!
//! So the expensive mistake this tool prevents is a real one, and it also
//! rescues a query that outlived `bq-query`'s polling window: that returns a job
//! id precisely so the results can be collected here once the job finishes.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_u64_loose, commas, decode_rows, human_bytes, parse_schema, render_rows, str_arg, u64_arg, Bq,
};
use serde_json::{json, Value};

const DEFAULT_MAX_ROWS: u64 = 200;
const MAX_CELL: usize = 120;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-results".to_string(),
            description: "Fetch results from a BigQuery query job that has already run, by \
                job id. Free: re-reads a completed job's output without re-running or \
                re-scanning it, so this — not repeating the SQL — is how to page through a \
                large result set or collect a query that was still running when `bq-query` \
                returned. Results are available for 24 hours after the job. Reaches \
                bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job id, as reported by `bq-query` or `bq-jobs`."
                    },
                    "start_index": {
                        "type": "integer",
                        "description": "Zero-based row to start at. The simplest way to page: ask for rows 200 onward."
                    },
                    "page_token": {
                        "type": "string",
                        "description": "Page token from an earlier call, as an alternative to start_index."
                    },
                    "max_rows": {
                        "type": "integer",
                        "description": "Rows to return. Default 200."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project the job ran in. Defaults to the configured project."
                    },
                    "location": {
                        "type": "string",
                        "description": "Job location, e.g. 'US'. Required for jobs outside the US/EU multi-regions."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["table", "json"],
                        "description": "Output shape. 'table' (default) is compact; 'json' is exact."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth access token, overriding any configured credential."
                    }
                },
                "required": ["job_id"],
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
        let mut job_id = str_arg(&args, "job_id").ok_or("missing required argument 'job_id'")?;

        // `bq-jobs` and the console show a job as project:location.jobId; accept
        // that whole string rather than making the caller dissect it.
        let mut location = str_arg(&args, "location").or_else(|| client.location.clone());
        if let Some((head, tail)) = job_id.clone().split_once('.') {
            if let Some((_, region)) = head.split_once(':') {
                location = Some(region.to_string());
                job_id = tail.to_string();
            }
        }

        let max_rows = u64_arg(&args, "max_rows").unwrap_or(DEFAULT_MAX_ROWS).max(1);

        let mut query = vec![("maxResults".to_string(), max_rows.min(1000).to_string())];
        if let Some(start) = u64_arg(&args, "start_index") {
            query.push(("startIndex".to_string(), start.to_string()));
        }
        if let Some(token) = str_arg(&args, "page_token") {
            query.push(("pageToken".to_string(), token));
        }
        if let Some(location) = &location {
            query.push(("location".to_string(), location.clone()));
        }
        // Do not block: a job that is still running should be reported as such,
        // not waited on here.
        query.push(("timeoutMs".to_string(), "0".to_string()));

        let response = client.get(&format!("/projects/{project}/queries/{job_id}"), &query)?;

        if !response
            .get("jobComplete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(format!(
                "job {job_id} is still running. Call again in a little while — nothing is \
                 lost, and the results will be free to read once it finishes."
            ));
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
            return Err(format!("job {job_id} failed: {message}"));
        }

        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let raw_rows = response
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let decoded = decode_rows(&fields, &raw_rows);
        let total = response
            .get("totalRows")
            .and_then(as_u64_loose)
            .unwrap_or(decoded.len() as u64);

        let mut out = String::new();

        if decoded.is_empty() {
            out.push_str("(no rows at that position)\n");
            if total > 0 {
                out.push_str(&format!(
                    "The result set has {} row(s); start_index may be past the end.\n",
                    commas(total)
                ));
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

        let start = u64_arg(&args, "start_index").unwrap_or(0);
        out.push('\n');
        out.push_str(&format!(
            "rows {}–{} of {} — free, the scan was billed when the job ran\n",
            commas(start + 1),
            commas(start + decoded.len() as u64),
            commas(total)
        ));

        if let Some(token) = response
            .get("pageToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            out.push_str(&format!(
                "more rows available: pass start_index={} or page_token={token}\n",
                start + decoded.len() as u64
            ));
        }

        if let Some(bytes) = response.get("totalBytesProcessed").and_then(as_u64_loose) {
            if bytes > 0 {
                out.push_str(&format!(
                    "(the original job scanned {})\n",
                    human_bytes(bytes)
                ));
            }
        }

        Ok(out)
    }
}

export!(Component);
