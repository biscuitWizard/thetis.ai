# Frontmatter field reference

Every field a skill's TOML frontmatter accepts, with its limits. Fetch this when
writing frontmatter and unsure of a name, a type, or a cap.

| Field | Type | Limit | Purpose |
|---|---|---|---|
| `name` | string | — | Human-readable title, for display |
| `brief` | string | **200 chars** | The L0 line. What this skill does |
| `when_to_use` | string | **1024 chars** | Trigger conditions, L1 only |
| `universal` | bool | 20 per corpus | Include in every system prompt |
| `tags` | array of strings | — | Retrieval terms; the lexical fallback's signal |
| `children` | `"auto"` or array | depth 3 | Nested skills |
| `related` | array of strings | — | Cross-references, not used for ranking |
| `version` | integer | — | Bump when the body changes meaningfully |

## Defaults

Everything except `name` and `brief` is optional.

- `universal` defaults to `false`.
- `children` defaults to `"auto"`, which adopts nested directories.
- `tags`, `related` default to empty.
- `version` defaults to 1.

## What counts as an error

`skill_lint` reports two severities:

- **error** — the skill is broken. An empty `brief` is the main one: without it
  there is no L0 text and the skill can never be retrieved.
- **warning** — the skill works but is weaker than it should be. An empty
  `when_to_use` leaves retrieval matching on the brief alone.

## Reserved names

`references`, `scripts` and `assets` are resource directories, so a nested skill
cannot take one of those names. `skill_write` refuses it: the skill and the
directory would occupy the same path and a fetch could not say which was meant.

## Which fields are indexed

Ranking sees `name`, `brief`, `when_to_use` and `tags`. It does **not** see the
body, `related`, or `version`.

Two consequences:

1. Editing a body does not change the skill's embedding, so it is not re-fetched
   from the provider. Cheap to revise prose.
2. Nothing in the body can make a skill retrievable. If it is not surfacing, the
   fix is always in the frontmatter.
