# Native mooR tools

The `moo-*` WebAssembly components replace the `moor-mcp-host` MCP operations with native Thetis tools over mooR web-host HTTP.

## Configuration

Set non-secret defaults in `thetis.toml` under `[tools.moo]`. Put credentials only in gitignored `thetis.local.toml` or environment overlays:

```toml
[tools.moo]
base_url = "http://10.10.10.1:7892"
username = "dedicated-programmer"
password = "..."
wizard_username = "dedicated-wizard"
wizard_password = "..."
timeout_secs = 10
request_timeout_secs = 30
```

Authentication uses `/auth/connect`; tokens are cached by base URL and identity. `moo-reconnect` clears and refreshes them. Tools retry authentication once after HTTP 401.

## Safety

- Execution, command, invocation, notification, task-kill, and write tools are mutating.
- Structurally read-only tools carry the `read-only` capability.
- Wizard-only command dispatch and objdef operations always use the wizard identity.
- Objdef file paths are confined beneath `workspace/torchship-objdef`; absolute paths, traversal, and symlink escapes are rejected.
- Captured deadlines are capped at 300000 ms. A timed-out mutation may already have committed side effects; inspect state before retrying.
- Web-host normal request bodies are limited to 1 MiB unless deployment configuration changes.

## Maintenance

`tools/moo-server-info/src/moo.rs` is canonical shared client code. After changing it:

```sh
cd tools/moo-server-info
bash sync-shared-client.sh
bash sync-shared-client.sh --check
```

Every component is standalone and must build for `wasm32-wasip2`.

## Intentional differences from MCP host

- Native names use hyphens (`moo-get-verb`) instead of underscores (`moo_get_verb`).
- Objdef file access is workspace-confined rather than arbitrary host filesystem access.
- Captured responses preserve committed output alongside success/error data.
- MOO-defined dynamic tools are exposed through `moo-dynamic-list`, `moo-dynamic-refresh`, and `moo-dynamic-invoke`; they do not register new Thetis manifests.
- `moo-diff-object` currently returns a line diff, not compiler-backed structural diff.
- `moo-test-compile` is not implemented natively. The MCP version links mooR's unpublished compiler and has no web-host operation; this project does not modify mooR or execute source merely to test compilation.

## Deployment dependency

Torchship web-host is `http://10.10.10.1:7892`. Native tools use the upstream API exposed by that deployment; no mooR source changes are part of this implementation.
