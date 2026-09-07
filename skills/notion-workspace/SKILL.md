---
name = "Working with Notion"
brief = "Read and write a Notion workspace with the notion-* tools: find ids, query databases, edit pages, comment."
when_to_use = "Use whenever a task involves Notion — reading a page, adding a row to a database, updating a status, leaving a comment, or searching a workspace. Also use when a notion-* tool returns 404, 403 or validation_error, since the causes are specific and the messages are terse. Not for the Notion desktop app or for general web research; notion-search matches page titles only, never page content."
universal = false
tags = ["notion", "database", "api", "pages", "comments", "workspace", "token", "tool-group:notion"]
children = "auto"
version = 2
---

# Working with Notion

Eleven tools, one credential. All of them take ids, and `notion-search` is the
only one that can find an id from a name — so most work starts there.

## Pick the tool

| Goal | Tool |
|---|---|
| Find a page or database by title | `notion-search` |
| Read a page's properties and body | `notion-page-get` |
| List databases with their data source ids | `notion-database-list` |
| See a database's columns and select options | `notion-database-schema` |
| Read rows, filtered and sorted | `notion-database-query` |
| Create a page or a database row | `notion-page-create` |
| Change properties, icon, trash state | `notion-page-update` |
| Change a page's text | `notion-page-content` |
| Read comments and discussion ids | `notion-comment-list` |
| Post a comment or reply | `notion-comment-add` |
| Check the token works | `notion-users whoami=true` |

## The two ideas that cause most failures

**1. Sharing is per-page, and nothing is shared by default.** A valid token sees
an empty workspace until someone opens a page in Notion and adds the connection
through the ••• menu → Connections. Children inherit that access. So a 404 means
"wrong id *or* not shared", and the second is more likely.

Diagnose with `notion-users whoami=true`. If that succeeds, the token is fine and
the problem is sharing — never keep retrying the same read.

**2. A database is not a data source.** Since API version 2025-09-03 a database
contains one or more data sources, and the data source is what holds the schema
and the rows. The ids look identical. Queries, schema reads and row creation all
want the **data source id**.

Get it from `notion-database-list`, which shows both ids side by side. The tools
recover from the mix-up where they can, but that costs an extra round trip.

## The normal sequence

Reading a page:

```
notion-search query="Roadmap"        → get the page id
notion-page-get page_id=<id>         → properties + markdown body
```

Writing to a database — do not skip the schema step:

```
notion-database-list                          → data_source_id
notion-database-schema data_source_id=<id>    → exact names, exact options
notion-page-create parent_data_source_id=<id> properties={...}
```

The schema call is what prevents a `validation_error`. Property names are
case- and space-sensitive, and a select or status property only accepts options
that already exist in the schema.

## Writing property values

Give plain values. The tools read the parent's schema and wrap them.

```json
{ "Status": "Done", "Priority": 3, "Tags": ["api", "urgent"],
  "Due": "2026-07-01", "Done": true, "Owner": ["<user-id>"] }
```

| Type | What to pass |
|---|---|
| title, rich_text, url, email, phone | a string |
| number | a number (a numeric string is parsed too) |
| checkbox | true / false |
| select, status | the option name, exactly as the schema spells it |
| multi_select | a list of option names, or one bare string |
| date | `"2026-07-01"`, or `{"start": "...", "end": "..."}` |
| people | a list of user ids |
| relation | a list of page ids |
| anything | `null` clears it |

Notion's fully-wrapped form is passed through untouched, so anything the
coercion misses stays reachable.

**Cannot be written at all:** formula, rollup, created_by, created_time,
last_edited_by, last_edited_time, unique_id. Notion computes these. Naming one
is caught before the request goes out, and the error lists the writable
properties — read that list rather than guessing again.

## Editing page text

`notion-page-content` has four modes and the choice matters.

1. Read the page first with `notion-page-get`, so the text you search for is
   exactly what is there.
2. Prefer `mode="edit"` with `edits=[{old_text, new_text}]`. It is surgical, and
   it fails loudly when `old_text` is absent rather than writing over something
   else.
3. `append` / `prepend` to add without touching what exists.
4. `replace` overwrites the entire body, needs `confirm_replace=true`, and Notion
   has no undo. Reach for it only when the whole page really is being rewritten.

An `old_text` that matches twice is refused unless `replace_all` is set on that
edit. That refusal is a feature — it means the edit was ambiguous.

Copy `old_text` from the tool's own output. Notion stores typographic quotes
(’ “ ”) and em dashes, so text retyped by hand often fails to match while
looking identical.

## Queries

Filters are Notion's own filter objects, and the inner key must match the
property's *type*:

```json
{"and": [
  {"property": "Status", "status": {"equals": "In progress"}},
  {"property": "Due",    "date":   {"before": "2026-08-01"}}
]}
```

A filter naming a property that does not exist is rejected. A filter with the
wrong *expectation* quietly matches nothing — so read an empty result as "check
the filter", not "the database is empty".

On a wide database pass `properties=["Name","Status"]`. It makes the query faster
and the output far shorter.

Results are capped and paginated. Every list tool ends by saying whether that was
all of them; when a `start_cursor` comes back, there is more, and treating a
partial result as complete is how wrong conclusions get drawn. Pass the cursor
back verbatim — never parse it.

## Comments

`notion-comment-list` returns **unresolved** comments only; the API does not
expose resolved ones, so an empty result does not mean nobody commented.

To reply inside an existing thread, pass the `discussion_id` from that listing.
Passing `page_id` instead starts a new top-level thread, which is a different
thing from replying.

Comment text is inline markdown only — bold, italic, code, links. Headings, lists
and code fences do not become blocks; they render as literal `#` and `-`
characters. Keep comments to a sentence or two.

**Authors usually show as ids, not names.** A personal access token may only look
up its own user, so both `/v1/users` and `/v1/users/{id}` return 403 and no name
lookup is possible. Ids still distinguish people within a thread. Real names need
an integration token with user-information capability.

## Setup

One block serves every tool, because a `notion-*` tool inherits `[tools.notion]`.
Two ways to supply the credential:

```toml
# thetis.local.toml — gitignored, which is where a real token belongs
[tools.notion]
token = "ntn_..."
```

Or from the environment, which keeps it out of the repository entirely:

```
NOTION_API_KEY=ntn_...          # lands as `token` in the notion scope
THETIS_TOOL_NOTION_VERSION=... # the explicit form, for any key
```

The environment wins over the file. Either way the value is read at startup, so
restart afterwards. Create a token at https://www.notion.so/my-integrations, and
enable comment capabilities there if comments are needed — they are off by
default.

Note that an `EnvironmentFile` in a systemd unit is read only when the service
starts: adding a variable to it does nothing until the service is restarted, not
merely the agent's own runtime.

## Reading errors

| Error | Meaning | Do this |
|---|---|---|
| 401 | Token wrong or missing | Check `token` in `[tools.notion]` or `NOTION_API_KEY` |
| 404 `object_not_found` | Wrong id, or not shared | `notion-users whoami=true`, then share the page in Notion |
| 403 `restricted_resource` | Capability not enabled, or a token-type limit | Comments are off by default; user listing is impossible for a personal token |
| 400 `validation_error` | Body does not match the schema, or `old_text` did not match | Re-read `notion-database-schema`, or re-copy the text |
| 429 | Rate limited | Wait, then retry; about three requests a second |
| 409 `conflict_error` | Concurrent edit | Retry once |

Ids may be given as a bare id, a dashed UUID, or a pasted Notion URL — all three
work everywhere, so there is no need to strip a URL by hand.
