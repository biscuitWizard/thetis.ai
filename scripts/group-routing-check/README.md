# group-routing-check

Checks the tool-group table and router in `agents/agent-core/src/groups.rs`
without needing a wasm host.

```sh
cd scripts/group-routing-check
python3 extract.py ../../agents/agent-core/src/groups.rs > src/table.rs
cargo run --quiet
```

Exits non-zero on any failure, and prints how many built-ins each scenario
would actually offer.

## Why an extractor rather than unit tests

`groups.rs` is a wasm guest: it imports `sys::log` and `sys::kv-get` from the
host, so `cargo test` against it needs the whole component harness. `extract.py`
copies the pure items — the table, `component_group`, `score`, `coverage_gaps`,
`in_table_order`, `repair_pin` — into a plain binary and stubs `sys::log` as a
`println!`.

It copies rather than restates. A test that retyped the group table would only
prove the copy self-consistent, which is the one thing that does not matter.

## What it guards

Table integrity: unique ids, no tool in two groups, prefix rules naming real
groups, `tool_search` in an always-on group, the `extra` fallback present and
always-on.

Coverage: `BUILTINS` in `src/main.rs` lists the built-in tool names, and the
check fails if one is in no group, or if a group names a tool that does not
exist. **Update that list when adding or removing a built-in** — a missing entry
is the one gap the harness cannot see for itself.

Routing: each scenario asserts groups that must be admitted and groups that must
not. The must-not half is the valuable half — it is what caught `selfmod` being
tagged `tool`/`tools`, which admitted the entire dev kit for any conversation
that so much as mentioned tooling.

The pin as untrusted input: the web gateway writes `__tool_groups` directly to
override the routing, so `repair_pin` is the only thing standing between a bad
writer and a conversation that cannot recover its tools. The checks assert that
always-on groups are forced back in (so `tool_search` is never losable), that
unknown ids are dropped, that order is normalised to the table's — a reordering
alone would miss the provider's prompt cache — and, most importantly, that an
empty or wholly unrecognisable pin reads as *unrouted* rather than as "route to
nothing". Getting that last one backwards is how a corrupt value turns into a
model with no tools at all; the same inversion in the gateway made the panel
report 0 of 75 tools attached when the truth was 75 of 75.

## Tag discipline

Routing is a keyword prefilter, so a tag earns its place only if it rarely
appears in conversations that do *not* want the group. Words dropped for failing
that test: `tool`, `tools`, `agent`, `self` (selfmod), `query`, `table`, `rows`,
`schema` (bigquery), `page`, `workspace`, `database`, `doc` (notion), `search`,
`documentation` (web), `commit`, `pull`, `push` (github), `sandbox`, `history`
(branch).

Losing a tag is cheap. A group is also admitted by any retrieved skill carrying
`tool-group:<id>`, and calling a tool from a withheld group admits it on the
spot. Keywords are the weakest of the three routes, so they should be the
pickiest.
