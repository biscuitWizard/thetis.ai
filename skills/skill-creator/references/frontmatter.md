# Frontmatter field reference

Every field a skill's TOML frontmatter accepts, with its limits. Fetch this when
writing frontmatter and unsure of a name, a type, or a cap.

| Field | Type | Hard limit | Aim for | Purpose |
|---|---|---|---|---|
| `name` | string | — | — | Human-readable title, for display |
| `brief` | string | **200 chars** | **160** | The L0 line. What this skill does |
| `when_to_use` | string | **1024 chars** | **400** | Trigger conditions, L1 only |
| `universal` | bool | 20 per corpus | — | Include in every system prompt |
| `tags` | array of strings | — | — | Retrieval terms; the lexical fallback's signal |
| `children` | `"auto"`, `"none"` or array | depth 3 | — | Nested skills |
| `related` | array of strings | — | — | Cross-references, not used for ranking |
| `status` | `"active"` or `"retired"` | — | — | Lifecycle. Retired skills still resolve |
| `superseded_by` | string (skill id) | — | — | Where a reader of a retired skill should go |
| `version` | integer | — | — | Bump when the body changes meaningfully |

The hard limit is what `skill_lint` errors on. The "aim for" column is what it
warns on: a brief that merely fits inside 200 characters is still unreadable in a
list of forty, and truncates in tool output where the brief is all that shows.

## Defaults

Everything except `name` and `brief` is optional.

- `universal` defaults to `false`.
- `children` defaults to `"auto"`, which adopts nested directories.
- `tags`, `related` default to empty.
- `status` defaults to empty, meaning active.
- `version` defaults to 1.

## Retiring a skill instead of deleting it

Delete a skill only when nothing ever referenced it. Otherwise retire it: every
body that links to it keeps resolving, and the linter tells each referrer where
to go instead.

```toml
status = "retired"
superseded_by = "torchship/world-simulation/status-effect"
```

That combination makes three things happen:

1. The retired skill's own body should open by saying so — it is still fetchable.
2. Any skill linking to it gets a warning naming `superseded_by`, so a stale
   cross-reference surfaces on the next lint rather than misleading a reader.
3. A `superseded_by` that matches no skill is an **error**, so the forwarding
   address cannot itself rot.

## The two severities

- **error** — a reader can be sent somewhere that does not exist, or cannot find
  something that does. Empty `brief`, a link matching no skill, a parent that
  omits a child, a missing H1, an unclosed code fence, a bad `status`.
- **warning** — the skill works but reads badly. Long `brief` or `when_to_use`,
  a shouty `NOTE:` label, a prose "See also" list, a link to a retired skill, a
  bare name that resolved but should be spelled in full.

## Reserved names

`references`, `scripts` and `assets` are resource directories, so a nested skill
cannot take one of those names. `skill_write` refuses it: the skill and the
directory would occupy the same path and a fetch could not say which was meant.

## Which fields are indexed

Ranking sees `name`, `brief`, `when_to_use` and `tags`. It does **not** see the
body, `related`, `status` or `version`.

Two consequences:

1. Editing a body does not change the skill's embedding, so it is not re-fetched
   from the provider. Cheap to revise prose.
2. Nothing in the body can make a skill retrievable. If it is not surfacing, the
   fix is always in the frontmatter.
