---
name = "Cohort retention analysis"
brief = "Calculate D1/D7/DN retention correctly and read a change over time without being fooled by mix shift or small cohorts."
when_to_use = "Use when computing retention, churn or cohort metrics from event data, and especially when asked whether a metric is up or down, or why it moved. Covers cohort SQL, definition choices, maturity filtering, and telling a real product change from a traffic-mix change. Works for any GA4/Firebase export. Not for a specific dataset's table names or auth — see that product's own metrics skill."
universal = false
tags = ["retention", "cohort", "d1", "d7", "churn", "ga4", "firebase", "analytics", "metrics", "significance", "mix-shift"]
related = ["nova-island-metrics"]
version = 1
---

# Cohort retention analysis

A retention number is three decisions and a denominator, not a fact. Get any of
the three wrong and the query still returns a plausible percentage, which is why
this is worth a procedure.

For a concrete dataset — table names, auth, the `fb-retention` tool — see
[nova-island-metrics](skill:nova-island-metrics).

## The three decisions, stated every time

1. **What starts a cohort.** `first_open` (install) for GA4, or the product's own
   `new_player`/signup event. These disagree: an install that never finishes
   onboarding is in one and not the other.
2. **What counts as active.** GA4's `is_active_user` flag is what the Firebase
   console counts and is the right default. "Any event" includes background
   pings and silently inflates the number.
3. **What "day N" means.** Active on *exactly* day N (console), or on day N *or
   later* (survival).

On one real 30-day window the third decision alone moved D7 from 4.17% to
11.23% — a 2.7x spread from wording. **Never report a retention figure without
its definition attached**; it is not interpretable alone.

## The query shape

```sql
WITH cohorts AS (          -- one row per user: their day 0
  SELECT user_pseudo_id, MIN(PARSE_DATE('%Y%m%d', event_date)) AS cohort_date
  FROM `proj.dataset.events_2*`
  WHERE _TABLE_SUFFIX BETWEEN <lo> AND <hi> AND event_name = 'first_open'
  GROUP BY 1
),
activity AS (              -- one row per user per active day
  SELECT DISTINCT user_pseudo_id, PARSE_DATE('%Y%m%d', event_date) AS d
  FROM `proj.dataset.events_2*`
  WHERE _TABLE_SUFFIX BETWEEN <lo> AND <hi> AND is_active_user
),
flags AS (                 -- collapse to one row per user, per-day booleans
  SELECT c.cohort_date, c.user_pseudo_id,
    MAX(a.d = DATE_ADD(c.cohort_date, INTERVAL 7 DAY)) AS ret_d7
  FROM cohorts c LEFT JOIN activity a USING (user_pseudo_id)
  GROUP BY 1, 2
)
SELECT
  COUNTIF(DATE_ADD(cohort_date, INTERVAL 7 DAY) <= <last_complete_day>) AS d7_denominator,
  COUNTIF(ret_d7) AS d7_retained
FROM flags
WHERE DATE_ADD(cohort_date, INTERVAL 7 DAY) <= <last_complete_day>
```

The `LEFT JOIN` then `MAX(...)` then `GROUP BY user` shape matters: aggregating
straight to cohort level double-counts a user active on several days.

### Maturity is the filter people forget

A cohort that installed yesterday cannot have a D7. Include it and its
structural zero is averaged in as though it were churn. **This is the single
most common cause of a retention number that looks too low.**

So DN's denominator only contains cohorts with N full days of observation
behind them. A consequence to state in the output: **D1 and D7 have different
denominators over the same window** — D7 draws on strictly older cohorts. That
is correct, not a bug, and someone will ask.

Bound activity by `last_complete_day`, never `CURRENT_DATE()`: today is still
being collected and reads as churn.

### Two GA4 export traps

- **`events_*` also matches `events_intraday_*`**, pulling partial data in.
  Use `events_2*` — and note `_TABLE_SUFFIX` is then `0260830`, not `20260830`,
  so slice the bounds with `SUBSTR(FORMAT_DATE('%Y%m%d', d), 2)`. Comparing
  unsliced matches **nothing** and returns a clean, silent zero rather than an
  error.
- **The activity scan must extend N days past the cohort window**, or a late
  return is invisible. The two ranges are not the same range.

## Reading a change over time

Never answer "up or down?" from two endpoints. Four steps, in order.

### 1. Get the whole series, not the two ends

One month per row. A single comparison cannot distinguish a slide from a step,
and the endpoint you were handed may itself be an outlier. In a real case the
month asked about (April, D7 8.8%) turned out to be the **year's high**, making
the fall look worse than the trend justified.

### 2. Check whether the mix changed

If volume moved sharply, suspect composition before product. Break the same
metric down by `traffic_source.medium`, platform and `app_info.version`:

| Aug by medium | installs | D1 | D7 |
|---|---|---|---|
| cpc (paid) | 1,210 | 17.97% | 3.23% |
| organic | 258 | 21.86% | 6.67% |

Paid users retained half as well and became 73% of installs, so the blended
average fell with **no change in the product**. This is Simpson's paradox in its
natural habitat: every segment can hold steady while the total drops.

### 3. Recompute with the mix held constant

Organic-only, the same two months read 8.80% → 6.62% for D7 — against a headline
fall of 9.01% → 4.17%. Report both, and say which one answers the question
asked. "Blended retention fell, per-segment retention did not" is usually the
honest headline.

### 4. Test significance before calling it a change

Small cohorts produce large swings. A two-proportion z-test costs one command:

```python
import math
def z2(r1, n1, r2, n2):
    p1, p2 = r1/n1, r2/n2
    p = (r1+r2)/(n1+n2)
    z = (p1-p2)/math.sqrt(p*(1-p)*(1/n1+1/n2))
    return p1, p2, z, math.erfc(abs(z)/math.sqrt(2))   # two-sided p
```

On the case above: headline D7 −4.8pp came out p<0.001 (real), while organic-only
D7 −2.2pp came out p=0.238 — **not** distinguishable from noise. Same data, and
the difference between "our game got worse" and "we bought cheaper users".

Daily cohorts of 10–25 users are noise at D7: one user is 4–10pp. Pool to
monthly or wider before drawing any conclusion from them.

## Verification

Compute the headline two ways and require agreement before reporting: once with
a tool or saved query, once hand-written from the shape above. On a first run
they disagreed because of the `_TABLE_SUFFIX` slicing trap, which returned zero
rows as "not measurable" rather than failing. Also check that segment
denominators sum to the pooled one — 1,519 Android + 144 iOS = 1,663 total was
what confirmed the platform filter was not dropping users.

## Known gaps

- No confidence intervals on the reported rate itself, only a test between two
  rates. For a single cohort, a Wilson interval is the right addition.
- The z-test assumes independent users; a device reinstalling appears twice and
  mildly violates that.
- Keying on device id (`user_pseudo_id`) rather than account means a reinstall is
  a new cohort member. Matches the Firebase console, but it is not "players".
- GA4 `traffic_source` is last-click at install, so paid/organic splits are
  approximate at the margins and cannot settle attribution disputes.
- Nothing here covers DAU/WAU/MAU or revenue cohorts.
