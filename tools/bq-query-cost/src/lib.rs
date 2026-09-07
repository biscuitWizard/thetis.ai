//! Estimate the cost of a BigQuery SQL query without running it (dry run).
//!
//! ## Why this is its own tool
//!
//! BigQuery charges by bytes *scanned*, and the charge lands whether or not the
//! result was what you wanted. A mistyped join across two partitioned tables
//! can scan terabytes and cost real money in one call. A dry run costs nothing,
//! validates the SQL, and reports the exact figure the real run would be billed
//! — so it belongs on the critical path before any unfamiliar query, and it has
//! to be cheap to reach for. Folding it into `bq-query` as a flag would bury
//! it; as its own tool it is one obvious call, and it can be declared
//! `read-only` and handed to a restricted mode with no risk at all.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    bool_arg, commas, estimate_cost, human_bytes, parse_schema, render_cost, statement_kind,
    str_arg, Bq,
};
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-query-cost".to_string(),
            description: "Dry-run a BigQuery SQL query: validate the syntax and find out \
                exactly how many bytes it would scan and what that would cost, without \
                running it and without being billed. Also returns the result schema, so it \
                doubles as a way to see what columns a query produces. Run this before any \
                query whose cost you are unsure of — BigQuery charges by bytes scanned and \
                bills you whether or not the result was useful. Reaches \
                bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The GoogleSQL query to price. Not executed. Legacy SQL is not supported."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project to run in. Defaults to the configured project."
                    },
                    "location": {
                        "type": "string",
                        "description": "Dataset location, e.g. 'US' or 'europe-west2'. Only needed when the data is outside the US/EU multi-regions."
                    },
                    "default_dataset": {
                        "type": "string",
                        "description": "Dataset assumed for unqualified table names, as 'dataset' or 'project.dataset'."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth access token to authenticate with, overriding any configured credential. `gcloud auth print-access-token` prints one."
                    },
                    "show_schema": {
                        "type": "boolean",
                        "description": "Include the columns the query would return. Default true."
                    }
                },
                "required": ["sql"],
                "additionalProperties": false
            })
            .to_string(),
            // A dry run creates no job, scans nothing and is never billed, so
            // this is genuinely read-only.
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
            .ok_or("missing required argument 'sql' — the query to price")?;

        let client = Bq::new(&config, &args);
        let project = client.project()?;

        let mut request = json!({
            "query": sql,
            // GoogleSQL. The API defaults this to *true*, which would silently
            // price a legacy-SQL parse of a modern query.
            "useLegacySql": false,
            "dryRun": true,
        });

        if let Some(location) = client.location.clone() {
            request["location"] = json!(location);
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

        let response = client.post(&format!("/projects/{project}/queries"), &request)?;

        let bytes = response
            .get("totalBytesProcessed")
            .and_then(bq::as_u64_loose)
            .unwrap_or(0);
        let cost = estimate_cost(bytes, client.price_per_tib);
        let kind = statement_kind(&sql);

        let mut out = String::new();
        out.push_str("valid — not executed, nothing billed\n\n");
        out.push_str(&format!("statement:   {}\n", if kind.is_empty() { "SELECT".into() } else { kind }));
        out.push_str(&format!(
            "would scan:  {} ({} bytes)\n",
            human_bytes(bytes),
            commas(bytes)
        ));
        out.push_str(&format!(
            "est. cost:   {} at ${:.2}/TiB on-demand\n",
            render_cost(cost),
            client.price_per_tib
        ));

        // A zero-byte scan is worth calling out: it means the query hit
        // metadata only, or the table is empty, and either way the reader
        // should not conclude "cheap because well-written".
        if bytes == 0 {
            out.push_str(
                "\nNote: zero bytes scanned. Either the query reads only metadata (a COUNT(*) \
                 on a native table, or a fully partition-pruned scan), or the table is empty.\n",
            );
        }

        if let Some(cap) = client.max_bytes_billed {
            if bytes > cap {
                out.push_str(&format!(
                    "\nWARNING: this exceeds the configured maximum_bytes_billed of {} — \
                     `bq-query` would refuse it.\n",
                    human_bytes(cap)
                ));
            }
        }

        if bool_arg(&args, "show_schema").unwrap_or(true) {
            let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
            if !fields.is_empty() {
                out.push_str(&format!("\nreturns {} column(s):\n", fields.len()));
                for field in &fields {
                    let note = field
                        .description
                        .as_ref()
                        .map(|d| format!("  — {}", bq::clip(d, 80)))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "  {:<28} {}{}\n",
                        field.name,
                        field.display_type(),
                        note
                    ));
                }
            }
        }

        // Statement type is only present for DDL/DML/scripts, and it tells the
        // reader this is not a plain SELECT before they run it for real.
        if let Some(statement) = response
            .get("statistics")
            .and_then(|s| s.get("query"))
            .and_then(|q| q.get("statementType"))
            .and_then(Value::as_str)
        {
            out.push_str(&format!("\nstatement type: {statement}\n"));
        }

        Ok(out)
    }
}

export!(Component);
