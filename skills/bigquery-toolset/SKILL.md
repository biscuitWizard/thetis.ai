---
name = "The BigQuery toolset"
brief = "Use and maintain the bq-* BigQuery tools: which tool answers which question, and why billing dictates the split."
when_to_use = "Use when querying or exploring BigQuery through the bq-* tools, when choosing between them, when a query cost more than expected, or when editing one of the nine crates under tools/bq-*. Not for other SQL engines."
tags = ["bigquery", "sql", "data", "cost", "tools"]
version = 1
---

# The BigQuery toolset

Nine tool components, `tools/bq-*`. They are split the way they are because
**BigQuery bills by bytes scanned**, and that single fact decides which tool is
correct far more often than the resource type does.

## Choosing a tool

Work down this list. The free tools are first because they are usually enough.

| Question | Tool | Cost |
|---|---|---|
| What datasets/tables exist? | `bq-list` | free |
| What columns, what size, how partitioned? | `bq-describe` | free |
| What does the data look like? | `bq-peek` | **free** |
| What would this query cost? | `bq-query-cost` | free |
| Show me rows 200-400 of a query I ran | `bq-results` | free |
| What is in this column — nulls, range, common values? | `bq-profile` | one scan |
| Anything needing filter/join/aggregate | `bq-query` | scan |
| What has been costing money? | `bq-jobs` | 10 MB min |
| Change data or schema | `bq-execute` | scan |

### The mistake this layout exists to prevent

`SELECT * FROM t LIMIT 10` to see what a table holds. On a columnar store `*`
references every column, so it is a **full scan, billed in full**, to look at ten
rows. `bq-peek` reads the stored rows through `tabledata.list`, bypassing the
query engine, and is not billed at all. On the GA4 table used for testing that
is the difference between free and a scan of every column of 129.

Corollary: never re-run SQL to page. The result set lives 24h and `bq-results`
re-reads it free; re-issuing the query scans and bills again.

## Facts that shape correct usage

- **`useLegacySql` defaults to *true*** in the REST API. Every tool sets it
  false. Any new call site must too.
- **10 MB minimum billing per query** — but *only* when bytes are actually
  scanned. Zero-byte DDL is genuinely free; `estimate_cost` exempts 0 for that
  reason. Do not "fix" it back into a floor.
- **`tables.list` returns no row counts or sizes.** Verified against the API.
  `bq-list` therefore omits those columns rather than printing placeholders; per
  table, `bq-describe` has them, free.
- **`INFORMATION_SCHEMA` needs a region qualifier**, spelled `` `region-us` ``,
  and `creation_time` is the partition column. `bq-jobs` composes both.
- **Wildcard tables** (`events_*`) are a query-engine feature. `tables.get` and
  `tabledata.list` 404 on them. `bq-peek` rejects them explicitly; `bq-profile`
  resolves the newest matching shard for its schema and deliberately discards
  that shard's `numBytes` so it cannot misinform the sampling decision.
- Filter `_TABLE_SUFFIX` on a sharded set, or the scan covers every shard. The
  test dataset has 1613.

## Why `bq-profile` is one query and not twelve

The aggregates for every column are independent, and the engine reads each
column once no matter how many functions are applied. So null rate, distinct
count, min/max, mean, stddev and quartiles for all columns come back in **one
row from one scan**. `APPROX_QUANTILES`/`APPROX_COUNT_DISTINCT` are sketch-based:
no sort, no shuffle, ~1% error, which is the right trade for "what does this
look like".

Top-K is a *second* query, because it needs a `GROUP BY` per column and cannot
fold into a single-row aggregate. It is issued only for columns the first pass
showed to have ≤50 distinct values, as UNION ALL branches ranked by
`ROW_NUMBER()` so one column cannot crowd out the rest. If it fails, the first
pass's results are still returned — losing the bulk of the answer to a failed
extra would be worse.

Repeated fields are skipped and *named as skipped*: `UNNEST` changes the row
count, so they cannot share the aggregate. Silently omitting columns would let
someone conclude a field is absent.

Distinctions the report keeps deliberately: empty string is not NULL; a wholly
null column says "entirely null" instead of five lines of NULL.

## Mutation

`bq-execute` is the only tool without `read-only` in its capabilities, so a
read-only mode withholds exactly it and the eight readers stay usable. It is the
mirror of the others — it *refuses a bare SELECT* and points at `bq-query`.

`confirm=true` is required only for statements that **destroy**: DROP, TRUNCATE,
ALTER, GRANT/REVOKE, `CREATE OR REPLACE`, a MERGE with a DELETE branch, and —
the valuable case — `DELETE`/`UPDATE` with no WHERE, which is legal, silent and
empties the table. INSERT and plain CREATE do not ask, because a confirmation
one passes reflexively has stopped being a guard.

A statement that outlives the poll window returns "still running… has NOT been
cancelled", never an error: it was submitted and will most likely take effect.

## Maintaining the crates

- Each tool is a standalone package (empty `[workspace]`, no path deps), so
  `src/bq.rs` is **duplicated byte-identically across all nine**. Change it once,
  then copy to the other eight and check one hash:
  `md5sum tools/bq-*/src/bq.rs | awk '{print $1}' | sort -u`
- Guest bindings are `thetis::grip::sys` / `thetis::grip::types`, **not** the
  `genesis::harness` path in the older docs.
- Stack proven on `wasm32-wasip2`: `waki` (HTTP), `rsa` + `sha2` + `base64`
  (RS256). No TLS crate — TLS is terminated host-side.
- Editing a `Cargo.toml` outside the pipeline needs `cargo generate-lockfile`,
  since builds pass `--locked`.
- A terminal `cargo build` produces an artifact but does **not** load it; only
  the pipeline swaps. Editing under `tools/` with file tools does, via the
  watcher.

## Auth

Both paths, tried in order: configured credential, else a per-call
`access_token`. Config lives in `[tools.bq]` and is inherited by every `bq-*`
tool through prefix merging, so it is written once:

```toml
[tools.bq]
credentials_path = "secrets/bq-adc.json"   # inlined by the host as *_contents
project = "market-party-289715"
location = "US"
max_bytes_billed = 53687091200
```

`credentials_path` resolves relative to the directory of `THETIS_LOCAL_CONFIG`
and must sit inside a secret root — `~/.config/gcloud/...` is **not** one, so the
ADC file was copied to `secrets/`. Both service-account keys (self-signed JWT,
no token exchange) and authorized-user ADC (refresh-token exchange, access token
cached in KV) work.
