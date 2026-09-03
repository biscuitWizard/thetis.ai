---
name = "Nova Island metrics"
brief = "Pull Nova Island retention, installs and DAU from BigQuery, and avoid the GA4 cohort traps that silently return wrong numbers."
when_to_use = "Use for any question about Nova Island's live product metrics — D1/D7 retention, cohorts, installs, DAU, monetisation — or when querying GCP project market-party-289715. Read it before hand-writing GA4 cohort SQL anywhere, since the wildcard, maturity and dry-run traps are general to GA4 exports. Not for the Nova Island game server code, and not for the mooR engine."
universal = false
tags = ["nova-island", "firebase", "ga4", "bigquery", "retention", "cohort", "analytics", "metrics", "dau"]
related = ["cohort-retention"]
version = 2
---

# Nova Island metrics

Everything lives in GCP project **`market-party-289715`** (US).

## The two data sources, and which to trust

| | `analytics_249556675` | `nova_island_analytics_prod` |
|---|---|---|
| What | real Firebase/GA4 export | in-house event pipeline |
| Shape | standard GA4, 217 cols, `events_YYYYMMDD` shards | `game_events(time, event, player_id, metadata STRING)` |
| Use for | retention, installs, DAU, platform splits | gameplay, matches, economy, IAP |

Sibling datasets `nova_island_analytics_dev` / `_staging` exist — **prod is the
one to report**. They are separate datasets rather than a flag on a column, so
there is no environment filter to remember.

### The app is called marketparty

`app_info.id` is `com.thirteengames.marketparty` — a legacy bundle id. It **is**
Nova Island: the events are `matchBegin_Ranked`, `screen_deck_builder`,
`ranked_up`, and a traffic source literally reads "Nova Island 1.9.0 New FTUE".
Do not go looking for a `novaisland` app; there isn't one.

Two streams: ANDROID `2598204948` (~90% of users), IOS `2114481305`.

### In-house `game_events` vocabulary

`new_player`, `session_start`, `session_end`, `card_purchased`,
`card_researched`, `professional_unlocked`, `feature_game_type_unlock`,
`pro_deck_unlocked`, `challenge_start`, `challenge_complete`,
`pvp_challenge_unlocked`. The session and `new_player` events only exist from
**2024-11-18** — earlier cohorts cannot be measured this way.

`metadata` is JSON-in-a-STRING: use
`JSON_EXTRACT_SCALAR(metadata, '$.player.is_bot')`. **Filter bots out**
(`= 'false'`) on the `match` table.

The `ETL_D1Retention` and `TMP_Match*` tables are **stale scratch**, not a live
report — `ETL_D1Retention` holds two rows, from 2024 and 2026. Ignore them and
recompute.

## Retention: use `fb-retention`

The `fb-retention` tool encodes the correct query. Do not hand-roll it. For the
methodology behind it — the definition choices, maturity filtering, and how to
read a change over time without being fooled by mix shift — see
[cohort-retention](skill:cohort-retention).

```
fb-retention                                  # D1/D7, console definition
fb-retention days=[1,7,30] by_cohort=true
fb-retention platform=IOS
fb-retention dry_run=true                     # price it first, free
```

### Four traps it exists to avoid

1. **Immature cohorts.** A cohort from yesterday cannot have a D7. Including it
   averages a structural zero into the headline, and this is the usual reason a
   retention figure looks too low. Count only cohorts that have had the full N
   days, and report the D1 and D7 denominators separately, because they differ.
2. **`events_*` also matches `events_intraday_*`**, pulling in today's partial
   day so the last day reads as churn. Use `events_2*` — and note
   `_TABLE_SUFFIX` is then `0260830`, not `20260830`, so slice the bounds with
   `SUBSTR(FORMAT_DATE('%Y%m%d', d), 2)`. Comparing unsliced matches **nothing**
   and returns a clean, silent zero.
3. **The activity scan must extend N days past the cohort window**, or a late
   return is invisible.
4. **A dry run puts its byte count in `statistics.query.totalBytesProcessed`.**
   Reading `statistics.totalBytesProcessed` yields 0 B and makes a real scan
   look free.

### The definition changes the answer by 2.7x

Measured on the 30 days to 2026-08-30:

| definition | D1 | D7 |
|---|---|---|
| exact day N, `is_active_user` — **console** | 18.46% | 4.17% |
| exact day N, any event | 24.17% | 5.79% |
| day N or later (survival) | 29.89% | 11.23% |

Always state the definition beside the number. `is_active_user` is GA4's own
engagement flag and is what the Firebase console counts; "any event" includes
background pings, which is not a returning player.

## The 2026 trend, and the paid-traffic caveat

Monthly, all traffic, console definition:

| | Jan | Feb | Mar | Apr | May | Jun | Jul | Aug |
|---|---|---|---|---|---|---|---|---|
| D1 | 23.8 | 24.8 | 23.8 | 25.5 | 24.9 | 24.1 | 22.2 | 18.5 |
| D7 | 5.1 | 6.4 | 6.6 | 8.8 | 6.1 | 7.6 | 6.4 | 4.2 |

Flat through July, then a step down in August — which was **not** a product
regression. A paid campaign began that month: 1,210 cpc installs at D1 17.97% /
D7 3.23% against organic 21.86% / 6.67%, on total installs 3.5x the prior
average. There was no `cpc` traffic at all in June or July.

Organic-only, April to August, D7 goes 8.80% → 6.62% (p=0.238, not significant)
while the blended headline goes 9.01% → 4.17% (p<0.001). D1 organic fell
25.51% → 19.67% (p=0.033), which is real and is the part worth investigating.

So **always split by `traffic_source.medium` before reporting a trend** on this
dataset. April is also a poor baseline: its D7 of 8.8% is the year's high, and
its daily cohorts were only 9–24 users.

## Cross-checking against in-house data

Cohorting on `new_player` and treating any `game_events` row as activity gives
**D1 15.58%, D7 1.89%** for the same window. Lower than the GA4 figure, and
correctly so: it demands a logged *gameplay* event rather than mere app
engagement. Same ballpark in the same direction is the signal you want — an
exact match would be surprising.

## Auth

`[tools.bq]` holds `credentials_path` (ADC copied into `secrets/`), `project`,
`location`, `max_bytes_billed`.

**Config scopes split on `-`**, so `[tools.bq]` reaches `bq-query` but *not*
`fb-retention`. A non-`bq-*` tool needs its own credential keys under its own
block, `[tools.fb-retention]`. This is the first thing to check when one tool
authenticates and a sibling does not.

When tokens fail: the VM service account mints tokens but has **no BigQuery
scope**, and the user credential expires under an org reauth policy
(`invalid_grant / invalid_rapt`). The fix is

```
gcloud auth application-default login --no-launch-browser \
  --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/bigquery
```

which needs a human with a browser. A service-account key with BigQuery Data
Viewer + Job User would remove that recurring interruption.

## Verification

```
fb-retention end_date=2026-08-30 window_days=30
```

should report D1 18.46% (307/1663) and D7 4.17% (59/1416). If the numbers come
back as "not yet measurable" with 0 users, trap 2 has regressed.

## Known gaps

- No DAU/WAU/MAU or stickiness tool yet; `fb-retention` covers cohorts only.
- No revenue or IAP metrics; the `iap`, `shop` and `currency` tables in the
  in-house dataset are unexplored.
- Retention is keyed on `user_pseudo_id` (device), not `user_id` (account), so a
  player reinstalling counts as a new cohort member. Matches the console.
- The in-house cross-check counts any `game_events` row as activity, which is
  looser than the GA4 engagement flag; it is a sanity check, not a second
  opinion of equal standing.
