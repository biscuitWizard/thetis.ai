---
name = "Maintaining the notion tools"
brief = "Edit, rebuild and re-verify the notion-* tool crates, including the shared client duplicated across all eleven."
when_to_use = "Use when changing the code of a notion-* tool: fixing a request shape, adding a tool, editing the shared notion.rs client, or adjusting an error hint. Not for merely using the tools to read or write a workspace — that is the parent skill."
universal = false
tags = ["notion", "tools", "maintenance", "build", "wasm", "tool-group:notion", "tool-group:selfmod"]
version = 1
---

# Maintaining the notion-* tools

Eleven standalone cargo packages under `tools/`, one per tool. There is no
workspace to hold a library, so `src/notion.rs` — the shared client — is
**duplicated in every crate**.

## The shared client

`tools/notion-search/src/notion.rs` is canonical. Never edit a copy.

```bash
cd tools/notion-search
# edit src/notion.rs, then:
./sync-shared-client.sh           # copy over the other ten
./sync-shared-client.sh --check   # report drift, exit 1 if any
```

Run `--check` before committing. Silent divergence is the failure mode that
matters: a fix applied to one copy leaves ten tools disagreeing about error
handling, and nothing catches it at build time.

## Building

Building through the dev-kit pipeline for eleven crates is slow. In a terminal,
from inside one crate's directory:

```bash
cd tools/notion-page-get
cargo build --release --target wasm32-wasip2 --target-dir ../../target-wasm
```

`-p <name>` does not work from `tools/` — each crate is its own package, not a
workspace member. Cold build ≈35s, incremental ≈1-2s.

All eleven at once:

```bash
cd tools
for d in notion-*/; do n=${d%/}; (cd "$n" && \
  cargo build --release --target wasm32-wasip2 --target-dir ../../target-wasm \
  >/dev/null 2>&1 && echo "ok $n" || echo "FAIL $n"); done
```

A terminal build only produces the artifact. To actually **load** a change, make
an edit through `patch_code` on that tool — that runs the pipeline, which builds,
validates and hot-swaps. A comment-only edit is enough to force a reload.

Careful: `write_code` replaces the whole file. Writing a placeholder to
`src/notion.rs` to trigger a reload destroys the copy on disk. If that happens,
`./sync-shared-client.sh` restores it.

## Unit tests

The shared client's pure functions have tests that run natively:

```bash
cd tools/notion-search
cargo test --release --target-dir ../../target-native
```

Test the pure helpers — URL shortening, id normalisation, property coercion.
Anything that needs the network belongs in a live check instead.

## Verifying against the live API

Compiling proves nothing about request shapes. Notion's field names are
inconsistent between sibling commands, and the docs do not spell all of them
out, so **confirm the JSON body with curl before writing the Rust**:

```bash
sudo -n bash -c 'set -a; . /etc/thetis.env; set +a
curl -s -X PATCH "https://api.notion.com/v1/pages/$PAGE/markdown" \
  -H "Authorization: Bearer $NOTION_API_KEY" \
  -H "Notion-Version: 2026-03-11" -H "Content-Type: application/json" \
  --data "{...}"'
```

A `validation_error` names the field it wanted — `body.update_content.content_updates
should be defined` is the API telling you the correct key. That is the fastest
route to the right shape.

Confirmed shapes for `PATCH /v1/pages/{id}/markdown`, each needing a top-level
`type` discriminator:

| Command | Payload key |
|---|---|
| `update_content` | `content_updates: [{old_str, new_str}]` |
| `replace_content` | `new_str` |
| `insert_content` | `content`, plus `position: {type: start\|end}` |

Do a write test on a scratch page you create and then trash, never on real
project data.

## Adding a tool

1. Copy the nearest existing crate directory and rename it. The directory name
   is both the cargo package name and the aspect name.
2. Copy `notion.rs` in with `sync-shared-client.sh`.
3. Set `capabilities` correctly in `describe()`: read-only tools declare
   `["http", "read-only"]`, mutating ones declare only `["http"]`. That flag is
   what makes plan and chat modes withhold the writers, so getting it wrong
   hands a read-only mode a tool that can delete a page.
4. Restart the orchestrator — aspects are discovered at startup.

## Config

Every `notion-*` tool inherits `[tools.notion]` through group-scope merging in
`Config::tool_config_json`, so the credential is named once. `NOTION_API_KEY` in
the environment maps onto `token`, and `THETIS_TOOL_NOTION_<KEY>` sets any key.
The environment wins over the file.
