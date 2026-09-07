# Setting up Thetis

Thetis is one native binary (the orchestrator) plus a handful of WebAssembly
components it builds and loads itself. There is no bundler, no package manager
beyond cargo, and no container runtime required.

Everything below works the same on Windows and Linux; where they differ, both
are shown.

---

## Multi-user web access

The default configuration uses `auth.mode = "local"`: no login page, one
implicit administrator, and loopback-only Host/Origin checks.

To enable accounts:

1. Run `printf '%s\n' 'a long password' | thetis hash-password --stdin`.
2. Put `[[roles]]` and `[[users]]` entries in `thetis.local.toml` (not the
   tracked `thetis.toml`). Set `[auth] mode = "users"` and `claim_unowned` to
   an administrator's user id.
3. For remote access, set `server.bind` and `server.public_origin`, then put
   Thetis behind a TLS reverse proxy which preserves `Host`.

Or, for the common case of one administrator to start with, run
`scripts/enable-users-auth.sh <user-id>` — it prompts for the password, writes
the overlay, checks the configuration loads, and tells you to restart.

Thetis itself serves plain HTTP and deliberately does not trust
`X-Forwarded-*`. Do not expose it directly to the public internet. Sessions use
an HttpOnly, SameSite=Lax cookie with sliding expiry. Conversations and recall
are owner-scoped. The workspace remains shared between accounts; deny its
capabilities to roles that must not see or modify it.

What signing in looks like: `/login` is host-rendered (no WebAssembly in its
path, so it works when the gateway guest is broken), the sidebar footer shows
the account with a **log out** link, and an expired login sends the tab back to
`/login` rather than leaving it "reconnecting…". An account whose role sets
`see_all_sessions = true` gets a switch beside **New chat** for everyone's
conversations; the sidebar is personal until it is pressed. An administrator
also gets a **control panel** button in the sidebar footer: a tab on the stage
with trunk's history, every conversation's worker, the accounts (live logins,
spend, sign out everywhere, and the `[[users]]` / `[[roles]]` entries
themselves), the model, provider and mode catalogues, and every setting in
`thetis.toml` with its help text and where its value comes from. Writes are
validated against the whole configuration before they land; accounts and
secrets go to `thetis.local.toml`, everything else to `thetis.toml`, and a
banner offers the restart that applies them. `/admin`, rendered by the
orchestrator with no WebAssembly in its path, keeps the same trunk, worker,
account and publishing controls as plain forms for when the UI itself is
broken.

To check an installation end to end, run the ignored live test against it with
two accounts (one admin, one plain user):

```sh
THETIS_WS_URL=ws://127.0.0.1:7777/ws \
THETIS_AUTH_ADMIN=alice:password THETIS_AUTH_USER=bob:password \
  cargo test -p thetis --test ws_auth -- --ignored --nocapture
```

---

## 1. Prerequisites

**Rust 1.82 or newer**, and the `wasm32-wasip2` target. That target is the only
unusual requirement: it is what lets plain `cargo build` emit a WebAssembly
*component*, which is why Thetis needs no `cargo-component` or similar tooling.

<details>
<summary><b>Windows</b></summary>

Install Rust from <https://rustup.rs> (the `rustup-init.exe` installer). It will
offer to install the MSVC build tools if they are missing — accept, as the
linker is required.

Then, in PowerShell:

```powershell
rustup target add wasm32-wasip2
```
</details>

<details>
<summary><b>Linux</b></summary>

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup target add wasm32-wasip2
```

You also need a C linker and OpenSSL-free TLS is already handled (Thetis uses
rustls), so on a minimal image install build essentials:

```bash
# Debian / Ubuntu
sudo apt-get install -y build-essential pkg-config

# Fedora / RHEL
sudo dnf install -y gcc make
```
</details>

Check both are present:

```bash
rustc --version
rustup target list --installed
```

You should see `wasm32-wasip2` listed. Thetis was developed against Rust 1.95.

**An OpenRouter API key** from <https://openrouter.ai/keys>. Thetis talks to
OpenAI-compatible endpoints, so any OpenRouter-supported model works.

You can also point Thetis at a local server — llama.cpp, vLLM, Ollama, LM
Studio — either instead of OpenRouter or alongside it, so local and hosted
models sit side by side in the model picker. Add a `[[providers]]` entry to
`thetis.toml`; the section there documents it. Note that a local model must
support tool calling, and llama.cpp's server needs `--jinja` for that to work
at all.

---

## 2. Get the code and build

```bash
cd thetis
cargo build --release
```

The first build compiles wasmtime and takes **several minutes**. Later builds
are seconds.

This builds only the orchestrator. The agent, the chat gateway and any tools are
guest components — the orchestrator compiles those itself on first run.

---

## 3. Give it a key

The key can go in the config file or the environment. **The environment wins**,
so you can keep one in a file and override it per run.

The file `thetis.toml` in the project root is read on startup. Every setting
in it is optional — delete the file entirely and Thetis still runs on its
built-in defaults.

<details>
<summary><b>Option A — environment variable (recommended)</b></summary>

Windows PowerShell, for the current session:

```powershell
$env:OPENROUTER_API_KEY = "sk-or-v1-..."
```

Windows, permanently:

```powershell
setx OPENROUTER_API_KEY "sk-or-v1-..."
```

Linux:

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."
# or add it to ~/.bashrc / ~/.zshrc to persist
```
</details>

<details>
<summary><b>Option B — a local config file</b></summary>

Put the key in a file that version control ignores, rather than in
`thetis.toml` itself:

```bash
cp thetis.toml thetis.local.toml
```

Edit `thetis.local.toml` and set, under `[llm]`:

```toml
api_key = "sk-or-v1-..."
```

Then point Thetis at it:

```bash
# Linux
THETIS_CONFIG=thetis.local.toml cargo run --release
```

```powershell
# Windows PowerShell
$env:THETIS_CONFIG = "thetis.local.toml"; cargo run --release
```

`.gitignore` already excludes `thetis.local.toml` and `*.local.toml`. It does
**not** exclude `thetis.toml`, so a key placed there would be committed.
</details>

Thetis holds the key in a type whose `Debug` prints `Secret(***)`, so it will
not reach a log line by accident.

---

## 4. First run

```bash
cargo run --release
```

Or run the binary directly:

```bash
# Linux
./target/release/thetis
```

```powershell
# Windows
.\target\release\thetis.exe
```

On the first start you will see it build each guest component:

```
INFO starting thetis root=/path/to/thetis
INFO building aspect=gateway/web
INFO loaded aspect=gateway/web revision=1 took_ms=...
INFO building aspect=agent
INFO loaded aspect=agent revision=1 took_ms=...
INFO hot reload active
INFO thetis listening addr=127.0.0.1:7777
```

Open <http://127.0.0.1:7777>.

Subsequent starts reuse the stored artifacts and come up in well under a second.

---

## 5. Trying it without a key

A stand-in server speaks enough of the streaming protocol to exercise the whole
system — token streaming, tool calls, usage accounting — at no cost. Useful for
development, and for confirming the setup works before spending anything.

Run it in one terminal:

```bash
cargo run --release --bin mock-llm
```

And Thetis in another, pointed at it:

```bash
# Linux
OPENROUTER_API_KEY=test \
OPENROUTER_BASE_URL=http://127.0.0.1:7788 \
THETIS_MODEL=mock/echo \
cargo run --release
```

```powershell
# Windows PowerShell
$env:OPENROUTER_API_KEY = "test"
$env:OPENROUTER_BASE_URL = "http://127.0.0.1:7788"
$env:THETIS_MODEL = "mock/echo"
cargo run --release
```

The mock picks its reply from keywords in your message — try "remember
something", "make a new tool", or "please be slow about this" to exercise
different paths.

To go back to the real API, **unset those three variables** (they override the
config file) and restart:

```powershell
# Windows
Remove-Item Env:OPENROUTER_BASE_URL, Env:THETIS_MODEL -ErrorAction SilentlyContinue
```

```bash
# Linux
unset OPENROUTER_BASE_URL THETIS_MODEL
```

---

## 6. Platform differences

Thetis behaves identically on both platforms except for these:

| | Windows | Linux |
|---|---|---|
| Terminal sessions use | PowerShell (`-NoLogo -NoProfile -NonInteractive -Command -`) | `sh -s` |
| Exec sandbox | Needs Docker Desktop | Needs Docker |
| Default paths | Same relative layout | Same relative layout |

The shell is configurable — set `terminal.shell` and `terminal.shell_args` in
`thetis.toml` to use `bash`, `pwsh`, `zsh`, or anything else.

The Docker exec sandbox is **off by default and not implemented yet**; while it
is off the agent is simply not offered code-execution tools rather than being
handed tools that fail. The host terminal and filesystem tools work on both
platforms without it.

---

## 7. Worth knowing before you start

**The agent can reach this machine.** Filesystem and terminal tools are enabled
by default, confined to the project root. That confinement is the real boundary:
paths are resolved with symlinks followed and must land inside
`filesystem.roots`. Enabling terminal access means the agent can run anything
you can — if that is not what you want, turn it off in `thetis.toml`:

```toml
[filesystem]
enabled = false

[terminal]
enabled = false

[control]
allow_restart = false
```

**It rebuilds itself while running.** Editing anything under `agents/`,
`gateways/`, `tools/` or `wit/` triggers a rebuild and a hot swap. A build that
fails is rejected and the previous version keeps serving.

**Everything is versioned.** `/admin` — served by the orchestrator with no
WebAssembly in its path — lists every component's revision history and every
whole-system snapshot, each with one-click restore. It keeps working when every
guest is broken, which makes it the thing to reach for if the agent breaks
itself.

---

## 8. Troubleshooting

**`error: failed to run custom build command` / linker errors on Linux**
Install build essentials (see prerequisites).

**`the 'wasm32-wasip2' target may not be installed`**
`rustup target add wasm32-wasip2`.

**`no API key: set llm.api_key in thetis.toml, or OPENROUTER_API_KEY in the environment`**
The key is missing or blank. Note that an *empty* setting counts as absent.

**Thetis refuses to start naming a config key**
The config file is parsed strictly, so a typo is an error rather than a setting
that silently does nothing. The message names the offending key.

**`Address already in use` on startup**
Another copy is running. Thetis retries the bind for 15 seconds to allow for
restarts, then gives up. Find and stop it:

```powershell
# Windows
Get-Process thetis | Stop-Process -Force
```

```bash
# Linux
pkill thetis
```

Or change `server.bind` in `thetis.toml`.

**`aspect failed to start aspect=tool/delete-tool`**
A leftover half-written tool that does not compile. It is harmless — the
orchestrator logs it and carries on — but you can delete `tools/delete-tool/` to
quiet it.

**The page says "the chat gateway is unavailable"**
The gateway component failed to build or load. The page is host-rendered and
links to `/admin`, where you can roll the gateway back to a working revision.

**Wiping state**
`data/` holds conversations and the revision registry; `artifacts/` holds every
built component. Deleting both resets Thetis to a clean install. Stop it first.

---

## 9. Where things live

```
thetis.toml             configuration; every setting documented inline
wit/thetis.wit          the host/guest contract — editing it rebuilds every guest
crates/thetis           the orchestrator
agents/agent-core        the agent's own source, which it can rewrite
gateways/gateway-web     the chat interface
skills/<name>.md         instruction sets attachable per conversation
tools/<name>             tools the agent scaffolds for itself
artifacts/               immutable revisions (component + source snapshot)
data/thetis.redb        conversations, event log, revision registry
```

See [README.md](README.md) for how the pieces fit together and what the agent
can do to itself.
