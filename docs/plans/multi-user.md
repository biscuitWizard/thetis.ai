# Multi-user mode for Thetis

Status: implemented on trunk (`c6625b6` and the follow-up that added the
"everyone's conversations" switch, the live `tests/ws_auth.rs`, and the
policy-aware UI). Kept as the design record; the operator's guide is
`skills/thetis-internals/multi-user/SKILL.md` and the users-mode section of
`SETUP.md`. Line numbers below are as of `c3b62d9` and have moved since.
Written 2026-09-02 against trunk `c3b62d9`.
Audience: the model (or person) implementing this. Every file path below is
real and every line number was checked against that commit; re-grep before
editing, because trunk moves.

---

## 1. What is being asked for

Four things, all of which the current system lacks:

1. **Accounts and a sticky login.** The web UI gets real user accounts with a
   password and a cookie session that survives reloads and reconnects.
2. **Conversation ownership.** A user sees only their own conversations. Every
   route to a conversation (sidebar, open, subscribe, transcript recall, branch
   panel, terminals, preview) is scoped to its owner.
3. **Tool scoping per account.** Some accounts may not use certain tools;
   some are read-only across the board.
4. **User-scoped configuration.** Which models, which modes, which default
   model, which capabilities, per user, with defaults inherited from the global
   `thetis.toml`.

## 2. What exists today, and what it means for the design

### 2.1 The trust boundary is "a process on this machine"

`crates/thetis/src/web.rs:51-58` says it in one layer: Thetis binds to loopback
and has no auth. `guard_local` (`web.rs:104-119`) refuses any `Origin` or
`Host` that is not loopback, which is what stops a hostile web page from opening
`/ws` or rebinding DNS at `/admin`. There is no notion of a user anywhere in the
kernel, the gateway guest, the agent, or the store. The README's "Not yet built"
list names "authentication for the web UI" as missing.

Consequence: authentication has to be added in the kernel, in `web.rs`, in
front of everything. It cannot live in the gateway guest, for the same reason
`/admin` is host-rendered: the guest is WebAssembly that the agent can rewrite
and hot-swap, so anything that gates access must sit on the far side of that
boundary. The login page therefore must be host-rendered too, so it keeps
working when the gateway component is broken.

Multi-user also implies the server is reachable from other machines, which
`guard_local` currently forbids. The Host/Origin guard has to become
configurable (a public origin) without losing the CSRF protection it provides.

### 2.2 The gateway guest runs unscoped, and the host is the attack surface

`gateway.rs:52-55` creates the guest's store with `session_id = None`, and
`host_api.rs:51-62` (`scope_ok`) lets an unscoped call touch any session:
"managing every session is their job". The guest calls `list_sessions`,
`get_session`, `events`, `submit`, `create_session`, `rename`, `archive`,
`set_session_mode`, `set_session_model`, `available_tools`, `kv_get`/`kv_put`
freely (`gateway-web/src/handlers.rs`).

`host_api.rs:1-5` states the design rule: "This module is the entire attack
surface a guest has against the system, so each function validates its
arguments, scopes access to the session the call was made for." That is the
lever. If the host knows *who* a gateway call is for, it can filter every one
of those imports by ownership, and the guest needs no changes to become
multi-user safe: `list_sessions` simply returns fewer rows, `events` on someone
else's session traps, `create_session` stamps the caller as owner.

The one place the guest's *output* must also be checked is the
`GatewayAction::Subscribe(session_id)` handling in `web.rs:1010-1012`: a
subscription is what routes broadcast frames to a socket, and the host inserts
whatever id the guest returns. Ownership has to be checked there, host-side.

### 2.3 `SessionMeta` is a WIT record and cannot be widened

`store.rs:57-63` and `subagents.rs:14-17` both record the lesson: `session-meta`
is shared with every guest and widening it breaks them at instantiation, so
parentage went into its own redb table (`SUBAGENTS`). Ownership goes the same
way: a new `OWNERS` table keyed by session id, never a field on `SessionMeta`.

### 2.4 Workers are untrusted and reach the store over IPC

`persist.rs:411-478` (`serve_store_call`) already pins a worker's mutating
calls to its own session or its sub-agents (`own_session`). Session management
calls (`list_sessions`, `create_session`, `get_session`) and the transcript
recall calls (`conversations`, `read_transcript`, `search_transcripts`,
`conversation_subagents`) are deliberately open across the whole database.

Under multi-user that openness becomes a leak: an agent in user A's
conversation could `grep_transcripts` user B's. `serve_store_call` knows
`caller_session`, and from it the gateway can derive the owner and filter every
open arm to that owner's conversations. This is a gateway-side change, so a
branch running a modified kernel cannot undo it.

Workers also need the *policy* for their session (which model is default, what
is denied) and they must not read it from their own checkout's `thetis.toml`,
because a branch can edit that file. The gateway resolves policy from its own
config and hands it to the worker over IPC, the same reason
`THETIS_DATA_DIR` is pinned over the environment at spawn (`workers.rs:1025-1027`).

### 2.5 How tools are offered and withheld today

Three mechanisms exist, and the design reuses all three:

- **Capability gates.** `agents/agent-core/src/tools.rs:46-74`: a tool family
  is offered only if the host says the capability exists (`hostfs::available`,
  `terminal::available`, `control::available`, `delegation::available`,
  `sys::config_get("devkit_available")`). Making those answers per-user makes
  whole families vanish from the prompt with no agent change. This is the
  README's invariant "a tool is offered only when the capability behind it
  exists".
- **Read-only modes.** `tools.rs:80-87` reads `read_only` off `sys::list_modes`;
  `available(mode)` drops mutating built-ins (`tools.rs:865`) and
  `definitions_for` / `manifests` drop components lacking the `read-only`
  capability (`tools.rs:1680-1684`, `1721-1726`); `invoke` refuses at dispatch
  (`tools.rs:1771-1775`). If the host reports every mode as `read_only` for a
  read-only user, the agent enforces it with no change.
- **Tool groups.** `groups.rs` scopes for attention, and the code is explicit
  that it is "never a permission boundary" (`tools.rs:1777-1783`). Do not use
  groups for authorization.

But the agent is rewritable. The *hard* boundary must be in the host imports:
`hostfs::write_file`, `terminal::open`, `devkit::*`, `control::restart`,
`configuration::set`, `branch::*` mutations, `delegation::spawn`,
`skills::upsert`, `tooling::invoke`, and, for models, `llm::stream_open`. Each of
those needs a policy check. Tool-name denials for built-ins (deny `terminal_run`
but allow `terminal_read`) can only be enforced by the agent, because built-ins
are agent-internal names the host never sees. The plan is honest about that:
capability-level denials are hard, per-name denials of built-ins are soft.

### 2.6 Config layering and validation

`config.rs:1858-1935` loads `thetis.toml`, merges `thetis.local.toml`, then the
shared overlay a worker is pointed at, then applies environment overrides in
`assemble`. Startup fails on unknown keys and on cross-references that do not
resolve (`default_mode` not in modes, profile naming a missing model:
`config.rs:2111-2160`). User and role definitions follow the same pattern:
parsed into `spec::*`, resolved into typed settings, validated at startup so a
role naming a model that is not in `[[models]]` is a boot error, not a
mid-conversation surprise.

`[[models]]` are `ModelSpec {id, label, provider, wire_model}`
(`config.rs:55-65`) and `[[modes]]` are `ModeSpec {id, label, description,
read_only, prompt}` (`config.rs:155-167`). Per-user config is an allow-list
over those ids plus a default, never a second copy of them.

### 2.7 Per-installation state that has to become per-user

Two KV keys are global today and are really per-person:

- `gateway.web.user_avatar` (`handlers.rs:464`, "Global scope: it is the
  person using the installation").
- `gateway.web.models`, the model-catalogue overlay (`handlers.rs:154`).

The host's `kv_get`/`kv_put` accept scope `"global"` or a session id
(`host_api.rs:164-182`). Adding a third scope word, `"user"`, which the host
rewrites to `user:<id>` of the principal (or of the session's owner, in an agent
store) gives both handlers a per-user home with a one-word change.

### 2.8 Discord already has half of this

`discord/policy.rs` has an allow-list, admin list, pairing, and per-user session
keys; `discord/mod.rs:1096-1119` creates sessions with a persisted
key-to-session map. It should keep working unchanged: sessions it creates get an
owner of `discord:<author_id>` and a configurable role, so they are neither
orphaned nor visible in every web user's sidebar.

### 2.9 Dependencies

`crates/thetis/Cargo.toml` has `sha2`, `hex`, `uuid` (v4), `axum 0.8` (ws),
`redb 4`. `rand`, `base64`, `subtle` are already in the lock transitively but not
direct. Nothing hashes passwords. Add `argon2 = "0.5"` (pure Rust, builds fine on
the native side; guests never see it). Cookies are parsed by hand from the
`Cookie` header, so no `axum-extra`.

---

## 3. Design

### 3.1 Principles

1. **Identity and authorization live in the kernel.** Never in a guest, never
   in a worker's checkout. The gateway process owns the listener, the database
   and the config it was started with, and it is the only thing that decides
   who a request is for and what they may do.
2. **Guests stay unchanged wherever the host can filter instead.** The gateway
   guest keeps calling the same imports; they answer differently per principal.
   The agent keeps asking `available()`; it gets policy-aware answers.
3. **Ownership is a table, not a field.** `OWNERS: session id -> user id`.
   Sub-agents resolve to their root's owner.
4. **Policy is three layers.** Global `thetis.toml` gives the universe (models,
   modes, capabilities on/off). A role narrows it. A user narrows or restates
   the role. Resolution happens once at startup into an `EffectivePolicy` per
   user, validated like profiles are today.
5. **Hard boundaries are capabilities and catalogues, checked in host imports.**
   Soft boundaries (per-name denial of a built-in) are published to the agent
   and enforced there, and documented as such.
6. **`auth.mode = "local"` is the default and changes nothing.** A single
   implicit principal (`local`, admin) is stamped as owner from now on, so a
   later switch to `users` mode has data to work with.

### 3.2 Vocabulary and types

```rust
// crates/thetis/src/auth.rs

/// Who a request or a guest call is for. Resolved once per HTTP request or
/// websocket connection from the cookie, and once per agent turn from the
/// session's owner. Cheap to clone; shared through Arc on HostState.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,       // "alice", "local", "discord:1234"
    pub display_name: String,
    pub role: String,
    pub policy: Arc<crate::policy::EffectivePolicy>,
}

impl Principal {
    pub fn is_admin(&self) -> bool { self.policy.admin }
}
```

```rust
// crates/thetis/src/policy.rs

/// A host-enforced capability family. One entry per group of host imports
/// that can change something or reach something. Coarse on purpose: every
/// entry here maps to a concrete `require()` call in host_api.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    FilesystemRead,
    FilesystemWrite,     // write_file, edit_file
    FilesystemDelete,    // delete_path
    Terminal,            // open/run/send/signal/close (local)
    Ssh,                 // remote terminals and the ssh_host_* registry
    Devkit,              // new_tool, write/patch, dependencies
    Control,             // restart_orchestrator
    ConfigWrite,         // configuration.set
    BranchWrite,         // update_from_trunk, reset_to, complete/abort merge
    Delegation,          // spawn_agent and friends
    SkillsWrite,         // skills.upsert / remove
    Transcripts,         // cross-conversation recall (own conversations only)
    ComponentTools,      // tooling.invoke for hot-loaded tool components
    Sandbox,
    Workspace,           // the shared workspace explorer, read
    WorkspaceWrite,      // ... and write/upload/delete
}

/// The resolved, validated policy for one user. Built at startup from
/// global config + role + user overrides; never mutated afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectivePolicy {
    pub admin: bool,
    /// Every session this user runs is read-only, whatever mode it names.
    pub read_only: bool,
    /// Capabilities withheld. `read_only` implies the writing ones (see
    /// `EffectivePolicy::denies`), but they are materialised here so a
    /// worker can read the answer without knowing the rule.
    pub denied: BTreeSet<Cap>,
    /// Model ids this user may pick. Always a subset of `[[models]]` plus
    /// any `extra_models` a role adds explicitly. Empty means none, which
    /// validation refuses.
    pub models: Vec<String>,
    pub default_model: String,
    /// Mode ids this user may pick. Subset of `[[modes]]`.
    pub modes: Vec<String>,
    pub default_mode: String,
    /// Tool names withheld. Trailing `*` is a prefix match. Hard for
    /// component tools (checked in `tooling.invoke`), soft for built-ins
    /// (published to the agent, which withholds and refuses them).
    pub deny_tools: Vec<String>,
    /// Tool-group ids never admitted for this user. Soft, agent-enforced.
    pub deny_groups: Vec<String>,
    /// Per-user spend ceiling in USD across all of their sessions. 0 = off.
    pub spend_limit_usd: f64,
    /// May this user delegate, and how wide.
    pub max_children: usize,
    /// May this user see every conversation, not just their own. Admins only;
    /// off by default even for admins so the sidebar stays personal.
    pub see_all_sessions: bool,
}

impl EffectivePolicy {
    pub fn denies(&self, cap: Cap) -> bool {
        if self.denied.contains(&cap) { return true; }
        if self.read_only {
            return matches!(cap,
                Cap::FilesystemWrite | Cap::FilesystemDelete | Cap::Terminal | Cap::Ssh
                | Cap::Devkit | Cap::Control | Cap::ConfigWrite | Cap::BranchWrite
                | Cap::SkillsWrite | Cap::WorkspaceWrite);
        }
        false
    }

    pub fn allows_model(&self, id: &str) -> bool {
        self.models.iter().any(|m| m == id)
    }

    pub fn allows_mode(&self, id: &str) -> bool {
        self.modes.iter().any(|m| m == id)
    }

    pub fn denies_tool(&self, name: &str) -> bool {
        self.deny_tools.iter().any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => pat == name,
        })
    }

    /// The policy `auth.mode = "local"` runs under: everything on.
    pub fn unrestricted(cfg: &Config) -> Self { ... }
}
```

### 3.3 Configuration

Everything under one new `[auth]` section plus `[[roles]]` and `[[users]]`.
Users with password hashes belong in `thetis.local.toml` (gitignored), which the
loader already merges (`config.rs:1876-1883`); the shipped `thetis.toml` carries
the roles and a commented example.

```toml
[auth]
# "local": today's behaviour. Loopback only, no login, one implicit admin
#          principal called "local". The default.
# "users": accounts below are required; every HTTP route except /login needs
#          a cookie; the server may be bound off loopback.
mode = "local"
# How long a login lasts without activity. Sliding.
session_ttl_hours = 720
# Conversations that predate ownership (or were created in local mode) are
# assigned to this user the first time users mode boots. Must name a user.
claim_unowned = "admin"
# The role Discord-originated conversations run under. Their owner is
# "discord:<author id>" and they never appear in a web user's sidebar.
discord_role = "reader"
# Failed logins per user before a cooling-off period, and its length.
lockout_after = 5
lockout_secs = 60

[server]
bind = "0.0.0.0:7777"
# Required in users mode when bound off loopback: the origin browsers will
# use, e.g. behind a reverse proxy that terminates TLS. Host and Origin
# headers must match it (or loopback). `https` here makes the cookie Secure.
public_origin = "https://thetis.example.com"

# A role is a named narrowing of the global configuration. Every key is
# optional and inherits from [llm], [[models]], [[modes]], [filesystem] etc.
[[roles]]
id = "admin"
admin = true
see_all_sessions = false

[[roles]]
id = "developer"
description = "Builds things, but may not restart or reconfigure the system."
# Subset of [[models]] ids. Omit for all of them.
models = ["anthropic/claude-sonnet-5", "local/deepseek-v4-flash"]
default_model = "anthropic/claude-sonnet-5"
modes = ["agent", "plan"]
default_mode = "agent"
# Capability families withheld, host-enforced. Ids: filesystem_read,
# filesystem_write, filesystem_delete, terminal, ssh, devkit, control,
# config_write, branch_write, delegation, skills_write, transcripts,
# component_tools, sandbox, workspace, workspace_write.
deny_capabilities = ["control", "config_write", "ssh"]
# Tool names withheld. Trailing * is a prefix. Hard for tool components,
# soft (agent-enforced) for built-ins.
deny_tools = ["moo-*", "bq-*", "delete_path"]
deny_groups = ["moo", "bigquery"]
spend_limit_usd = 25.0
max_children = 4

[[roles]]
id = "reader"
description = "Asks and reads. Changes nothing anywhere."
read_only = true
modes = ["plan", "chat"]
default_mode = "chat"
models = ["anthropic/claude-sonnet-5"]
deny_capabilities = ["transcripts", "delegation", "workspace_write"]
spend_limit_usd = 5.0

# In thetis.local.toml:
[[users]]
id = "alice"
name = "Alice"
role = "admin"
# Produced by `thetis hash-password`. Or `password_env = "THETIS_PW_ALICE"`
# to keep even the hash out of the file.
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$..."

[[users]]
id = "bob"
name = "Bob"
role = "developer"
password_hash = "..."
# Per-user overrides use exactly the role's keys and win key by key.
[users.overrides]
deny_tools = ["moo-*", "bq-*", "delete_path", "git-commit"]
```

Validation at startup (`Config::assemble`), all boot errors:

- every `[[roles]]` id unique and non-empty; every `[[users]]` id unique,
  non-empty, matching `[a-z0-9._-]{1,64}`, and naming an existing role;
- `models` entries exist in `[[models]]`, `default_model` is in the resolved
  list, and the list is non-empty; when omitted the list is all of `[[models]]`
  and the default is `llm.model` (if `llm.model` is not in the list, the first
  listed model becomes the default and a warning is logged);
- `modes` entries exist in `[[modes]]`, the default is among them; with
  `read_only = true` every mode in the list is treated as read-only whatever
  its own flag says;
- `deny_capabilities` values parse as `Cap`;
- in `users` mode: at least one user, at least one admin user,
  `claim_unowned` names a user, and every user has exactly one of
  `password_hash` / `password_env` (an env name that is unset is a boot error);
- in `users` mode with a non-loopback bind, `server.public_origin` is required.

Resolution order for each field: global default, then role, then
`users.overrides`. Lists replace, they do not merge, so a user override of
`deny_tools` is the whole list.

### 3.4 Storage

New redb tables in `store.rs` (created in `Store::open` beside the others):

```rust
/// session id -> owning user id. Sub-agents are not entered: their owner is
/// their root's, resolved through SUBAGENTS.
const OWNERS: TableDefinition<&str, &str> = TableDefinition::new("owners");
/// sha256(token) hex -> LoginRow (json)
const LOGINS: TableDefinition<&str, &[u8]> = TableDefinition::new("logins");
/// user id -> cumulative USD spend across every session they own
const USER_SPEND: TableDefinition<&str, f64> = TableDefinition::new("user_spend");
```

```rust
#[derive(Serialize, Deserialize)]
pub struct LoginRow {
    pub user_id: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
    pub expires_ms: u64,
    pub user_agent: String,
}
```

Store methods to add:

```rust
impl Store {
    pub fn set_owner(&self, session_id: &str, user_id: &str) -> Result<()>;
    pub fn owner_of(&self, session_id: &str) -> Result<Option<String>>;
    /// Sessions owned by `owner`; `None` means every session (admin/all).
    pub fn list_sessions_owned(&self, owner: Option<&str>, include_archived: bool)
        -> Result<Vec<SessionMeta>>;
    /// Every session with no OWNERS row. For the one-time claim at boot.
    pub fn unowned_sessions(&self) -> Result<Vec<String>>;

    pub fn put_login(&self, token_hash: &str, row: &LoginRow) -> Result<()>;
    pub fn get_login(&self, token_hash: &str) -> Result<Option<LoginRow>>;
    pub fn touch_login(&self, token_hash: &str, now_ms: u64, expires_ms: u64) -> Result<()>;
    pub fn remove_login(&self, token_hash: &str) -> Result<()>;
    pub fn remove_logins_for(&self, user_id: &str) -> Result<usize>;
    pub fn prune_expired_logins(&self, now_ms: u64) -> Result<usize>;

    pub fn get_user_spend(&self, user_id: &str) -> Result<f64>;
    pub fn add_user_spend(&self, user_id: &str, usd: f64) -> Result<f64>;
}
```

`create_session` gains an owner parameter:

```rust
pub fn create_session(&self, title: Option<String>, mode: &str, owner: &str) -> Result<SessionMeta>
```

and writes the OWNERS row in the same transaction. Callers to update:
`store.rs` tests, `persist.rs:61-68` and its `serve_store_call` arm
(`persist.rs:492-498`), `host_api.rs:364-387`, `delegation.rs:249-253`,
`discord/mod.rs:1113-1116`, `roles/gateway.rs:83-89`, `transcripts.rs` tests.

`Store::list_sessions` stays as the "every session" primitive for internal
callers (`reconcile_interrupted_turns`, `/admin`); everything user-facing goes
through `list_sessions_owned`.

The owner of a sub-agent session: `owner_of(root_of(session))`. Add a helper on
`Store`:

```rust
/// The user a session belongs to, walking a sub-agent up to its conversation.
pub fn owner_of_root(&self, session_id: &str) -> Result<Option<String>> {
    let root = crate::subagents::Subagents::new(self).root_of(session_id);
    self.owner_of(&root)
}
```

KV scope `user`: no table change. `host_api::kv_get/kv_put` map the literal
scope `"user"` to `format!("user:{id}")` where `id` is the principal's user id
(gateway store) or the session owner's (agent/tool store). A guest may not name
a `user:` scope directly: `scope.starts_with("user:")` is refused, so a guest
cannot read another user's preferences by guessing.

### 3.5 Authentication

New module `crates/thetis/src/auth.rs`. Responsibilities: password hashing and
verification, login tokens and the cookie, cookie parsing, the principal
resolver, the host-rendered login page, a lockout counter, and the `local`
principal.

**Password hashing.** Argon2id with the crate's defaults; hashes are PHC
strings. `thetis hash-password` (new subcommand in `main.rs`) reads a password
from stdin without echo (or from `--stdin` for scripts) and prints the hash. The
verifier accepts any PHC string `argon2` can parse, so parameters may change
later without a migration.

**Tokens.** 32 random bytes from `OsRng`, base64url without padding in the
cookie; the store holds `hex(sha256(token))` so a copy of the database does not
yield live sessions. Sliding expiry: `touch_login` on any authenticated request
at most once a minute (rate-limited in memory to keep redb writes off the hot
path).

**Cookie.** `thetis_session=<token>; Path=/; HttpOnly; SameSite=Lax;
Max-Age=<ttl>` plus `Secure` when `server.public_origin` is `https`. `Lax` on
purpose: the post-login redirect to `/` and the websocket upgrade are same-site
GETs and carry a Lax cookie; a cross-site POST or a cross-site websocket
handshake does not, and the Origin check refuses those regardless.

**Principal resolution** (`auth::resolve(grip, &HeaderMap) -> Option<Principal>`):

- `auth.mode = local`: always `Some(Principal::local(cfg))`, an admin with
  `EffectivePolicy::unrestricted`.
- `auth.mode = users`: parse the cookie, hash, `get_login`, reject if expired,
  look the user up in `cfg.auth.users`, build the principal from the user's
  resolved policy. A login row whose user no longer exists in config is
  removed and treated as unauthenticated (an account was deleted).

**Routes** in `web.rs::serve`:

| Route | Users mode | Local mode |
|---|---|---|
| `GET /login` | host-rendered form; already logged in -> 302 `/` | 302 `/` |
| `POST /login` | form `user`, `password`, `next`; on success set cookie, 302 `next` (same-origin paths only) or `/`; on failure re-render with a message and a 1s delay; lockout after N failures per user | 404 |
| `POST /logout` | delete the login row, clear cookie, 302 `/login` | 404 |
| `GET /api/me` | `{id, name, role, admin, read_only, capabilities}` as JSON, 401 when not logged in | the local principal |
| everything else | requires a principal; HTML navigations 302 to `/login?next=...`, others 401 | as today |
| `/admin*` | additionally requires `admin` | as today |
| `/ws` | principal captured at upgrade and passed into `connection` | local principal |
| `/preview/{session}/*` | owner or `see_all_sessions` | as today |
| `/workspace/file/*` | `Cap::Workspace` for GET, `Cap::WorkspaceWrite` for PUT | as today |

The middleware order is: `guard_origin` (the renamed `guard_local`), then
`authenticate`, which stashes `Principal` in request extensions for handlers to
read with `Extension<Principal>` (or `req.extensions().get()` inside a
`from_fn` middleware).

**Origin guard in users mode.** `guard_local` becomes `guard_origin`:

```rust
fn allowed_authority(cfg: &Config, authority: &str) -> bool {
    is_loopback_authority(authority)
        || cfg.public_origin.as_ref().is_some_and(|o| o.authority == authority)
}
```

applied to `Host` and to `Origin` exactly as today. Local mode keeps the current
loopback-only behaviour byte for byte (the existing `guard_tests` must keep
passing unchanged). Thetis still speaks plain HTTP; TLS is a reverse proxy's
job, and the plan does not add proxy-header trust (`X-Forwarded-*`) in this
round. Say so in `thetis.toml`.

**Lockout.** In-memory `Mutex<HashMap<String, (u32, Instant)>>` keyed by
user id: after `lockout_after` failures within `lockout_secs`, `POST /login`
answers "try again later" without checking the password. Cleared on success.

**Bootstrapping users mode.** No first-run wizard. The operator runs
`thetis hash-password`, writes a `[[users]]` block into `thetis.local.toml`, and
restarts. A users-mode boot with zero users is a config error with a message
that says exactly that.

### 3.6 Propagating the principal

`HostState` (`runtime.rs:173-188`) gains two fields:

```rust
pub struct HostState {
    ...
    pub session_id: Option<String>,
    /// Who this call is for. `Some` for every gateway store made on behalf of
    /// a websocket client or HTTP request; `None` for probes, renderers and
    /// tool describes, which touch no user data.
    pub principal: Option<Arc<Principal>>,
    /// The policy every import checks. Unrestricted for a probe; the
    /// principal's in a gateway store; the session owner's in an agent or
    /// tool store.
    pub policy: Arc<EffectivePolicy>,
}
```

`Runtime::new_store` keeps its signature and sets both to `None` /
unrestricted; two helpers on `Grip` set them:

```rust
impl Grip {
    /// A store for a gateway call made on behalf of `principal`.
    pub fn gateway_store(self: &Arc<Self>, budget: Budget, principal: Arc<Principal>)
        -> Store<HostState>
    {
        let mut store = self.runtime.new_store(self.clone(), Caps::Gateway, budget, None);
        store.data_mut().policy = principal.policy.clone();
        store.data_mut().principal = Some(principal);
        store
    }

    /// A store for an agent turn or a tool call in `session_id`, carrying the
    /// session owner's policy. Async because a worker fetches the policy
    /// from the gateway the first time it sees a session.
    pub async fn session_store(self: &Arc<Self>, caps: Caps, budget: Budget, session_id: &str)
        -> Store<HostState>
    {
        let policy = self.policy_for_session(session_id).await;
        let mut store = self.runtime.new_store(self.clone(), caps, budget, Some(session_id.to_string()));
        store.data_mut().policy = policy;
        store
    }
}
```

`Grip::policy_for_session`:

- gateway role: `owner_of_root(session)` on the local store, then
  `cfg.auth.policy_for(user_id)`; an unowned session (should not happen after
  the boot claim) gets the `claim_unowned` user's policy in users mode and the
  unrestricted policy in local mode;
- worker role: `persist.session_policy(session)`, a new `store.session_policy`
  IPC arm served by the gateway and pinned with `own_session` (a worker may ask
  only about its own session or its sub-agents). Cached in a
  `Mutex<HashMap<String, Arc<EffectivePolicy>>>` on the grip for the worker's
  life; policy comes from config, which only changes with a restart, and a
  worker never outlives one.

`gateway.rs::on_client_message` and `serve_asset` take a `&Arc<Principal>` and
use `gateway_store`. `Renderer` (render-event only) keeps `new_store` with no
principal. `serve_preview_asset` takes the principal for symmetry, though
`serve_asset` touches no user data today.

`web.rs::connection` receives the principal captured at upgrade and passes it to
every `on_client_message`. The host-side frame handlers (`branch_api`,
`workspace_api`, `debug_api`, `system_api`) get a `&Principal` parameter and
check ownership of the `id` they are given before doing anything (a small
helper `auth::may_access(grip, &principal, session_id) -> Result<()>`).

Before `watching.write().await.insert(session_id)` in `web.rs:1010-1012`, call
`may_access`; on refusal log at warn and send an `error` frame instead of
subscribing. The same check guards `Unsubscribe` only for tidiness.

On websocket open, before any guest frame, the host sends a `user` frame:

```json
{"type":"user","id":"bob","name":"Bob","role":"developer","admin":false,
 "read_only":false,"workspace":"write","see_all":false}
```

This is host business like `resync`, so the guest needs no `whoami` import. If
a later change wants the guest to render identity in HTML, add
`sys.whoami: func() -> user-info` through the `host-staging` world; not needed
for this round.

### 3.7 Enforcement matrix

Every row is a `require` call at the top of a host import, after
`self.budget.entered_host(..)`. The helper:

```rust
impl HostState {
    /// Refuses a call the caller's policy withholds. A refusal is a trap for
    /// the guest — the same shape as `scope_ok` — because a guest that
    /// reaches a withheld import has already ignored the answer `available()`
    /// gave it, and a soft error would just be worked around.
    fn require(&self, cap: Cap) -> Result<()> {
        if self.policy.denies(cap) {
            return Err(err(format!(
                "{cap:?} is withheld for this user by policy"
            )));
        }
        Ok(())
    }
}
```

| Import (`host_api.rs`) | Check |
|---|---|
| `sys.kv_get/kv_put` | scope `"user"` rewritten; `user:*` literal refused; writes to `"global"` from a gateway store need `admin` (the agent's own global table publishes, `groups.rs:880`, run in agent stores and are unaffected) |
| `sys.config_get("model")` | returns `policy.default_model` |
| `sys.config_get("devkit_available")` | `cfg.devkit.enabled && !denies(Devkit)` |
| `sys.config_get("policy_read_only")` | new: `"true"`/`"false"` |
| `sys.config_get("policy_deny_tools")` | new: comma-joined `deny_tools` |
| `sys.config_get("policy_deny_groups")` | new: comma-joined `deny_groups` |
| `sys.config_get("user_id" / "user_name")` | new: from principal or session owner |
| `sys.list_models` | filtered to `policy.models`, in config order |
| `sys.list_modes` | filtered to `policy.modes`; `read_only = true` on every entry when `policy.read_only` |
| `session.list_sessions` | `list_sessions_owned(principal or owner)`; `see_all_sessions` lifts the filter; an agent store lists only its owner's |
| `session.get_session`, `events`, `rename`, `archive`, `submit`, `set_session_mode`, `set_session_model`, `available_tools` | `may_access(session)`: existing `scope_ok` for agent stores, owner check for gateway stores |
| `session.create_session` | stamps `principal.user_id` as owner; still refused in agent stores |
| `session.set_session_mode` | additionally `policy.allows_mode(mode)` |
| `session.set_session_model` | additionally `policy.allows_model(model)`; empty (clear override) always allowed |
| `llm.chat`, `llm.stream_open` | parse the request's `"model"`; refuse with `LlmError::BadRequest` when not allowed; also `check_budget` against `policy.spend_limit_usd` using `USER_SPEND` |
| `llm.stream_next` `record_usage` | also `add_user_spend(owner, cost)` |
| `tooling.invoke` | `require(ComponentTools)`; `policy.denies_tool(name)` -> `Err` result (not a trap: the model should read the refusal); in read-only, refuse a component without the `read-only` capability |
| `hostfs.available` | `!denies(FilesystemRead)` |
| `hostfs.read_file`, `read_file_range`, `list_dir`, `search_files`, `find_files` | `require(FilesystemRead)` |
| `hostfs.write_file`, `edit_file` | `require(FilesystemWrite)` |
| `hostfs.delete_path` | `require(FilesystemDelete)` |
| `terminal.available` | `!denies(Terminal)` |
| `terminal.open` | `require(Terminal)`; `require(Ssh)` when `spec.host` is set |
| `terminal.run/read/send/signal/close/sessions` | `require(Terminal)` |
| `terminal.ssh_available` | `!denies(Ssh)`; `ssh_hosts*` -> `require(Ssh)` |
| `control.available` | `!denies(Control)`; `restart` -> `require(Control)` |
| `configuration.settings/get` | open (already secret-free) |
| `configuration.set` | `require(ConfigWrite)` |
| `devkit.*` mutating (`new_tool`, `write_file`, `patch_file`, `add/remove_dependency`) | `require(Devkit)` |
| `devkit.read_file/list_files/list_dependencies` | open |
| `branch.status/log` | open |
| `branch.update_from_trunk/reset_to/complete_merge/abort_merge` | `require(BranchWrite)` |
| `delegation.available` | `!denies(Delegation)` and the existing child check |
| `delegation.spawn` | `require(Delegation)`; `limits().max_children = min(cfg, policy.max_children)`; child inherits the parent's owner via `create_session` on the gateway (see 3.8) |
| `skills.upsert/remove` | `require(SkillsWrite)` |
| `transcripts.*` | `require(Transcripts)`; scoped to the owner's conversations on the gateway (3.8) |
| `sandbox.*` | `require(Sandbox)` (currently stubbed; keep the check so it is there when the sandbox lands) |

Host-side frames (`web.rs` dispatch):

| Frame family | Check |
|---|---|
| `branch-*` | `may_access(id)`; mutations (`merge`, `update`, `reset`, `resolve`, `abort`, `base`) `require(BranchWrite)` |
| `workspace-list/read/find` | `require(Workspace)` |
| `workspace-write/mkdir/delete/move`, `PUT /workspace/file` | `require(WorkspaceWrite)` |
| `debug-request`, `terminals`, `terminal-close`, `turn-cancel` | `may_access(id)` |
| `system-status` | any principal; the `sessions` count becomes the principal's own count |
| `/admin`, `/admin/*` | `admin` |

### 3.8 Ownership across the process split

`serve_store_call` (`persist.rs:439-658`) derives `caller_owner` once:

```rust
let caller_owner: Option<String> = if caller_session.is_empty() {
    None                                   // the test grip
} else {
    store.owner_of_root(caller_session)?   // None only for a legacy row
};
```

and changes these arms:

- `store.create_session`: owner = `caller_owner` (a worker creates sessions only
  through delegation, and the child belongs to whoever owns the parent). With
  no `caller_owner` (test grip) the params may carry `"owner"`.
- `store.list_sessions`: `list_sessions_owned(caller_owner, ..)`.
- `store.get_session`: refused unless owned by `caller_owner` or a sub-agent
  under one of their conversations.
- `store.conversations`, `store.conversation_subagents`,
  `store.read_transcript`, `store.search_transcripts`: `Transcripts` gets an
  `owner: Option<&str>` filter threaded through `conversations` and applied as
  an ownership check in `read`/`search`/`subagents`. Sub-agents are matched by
  their root's owner.
- `store.session_policy` (new): `own_session` then
  `grip.cfg.auth.policy_for(owner)` serialised as JSON.
- `store.get_user_spend` / `store.add_user_spend` (new): pinned to
  `caller_owner`.

`Persist` gains the matching methods with the `delegate!` pattern.

`Transcripts` change (`transcripts.rs:226-244`):

```rust
pub fn conversations(&self, owner: Option<&str>, include_archived: bool,
                     include_subagents: bool, limit: usize) -> Result<Vec<ConversationSummary>> {
    let owners = self.store.owners_map()?;   // one read txn: session id -> user
    let subagents_root = |id: &str| Subagents::new(self.store).root_of(id);
    let mine = |id: &str| match owner {
        None => true,
        Some(u) => owners.get(&subagents_root(id)).map(String::as_str) == Some(u),
    };
    ...filter(|(meta, _)| mine(&meta.id))...
}
```

The module's existing "holds no write path" test stays true.

**Boot-time claim.** In `roles/gateway.rs::run`, after `Store::open` and
before the first-session creation: in users mode, `unowned_sessions()` are
assigned to `auth.claim_unowned` and the count is logged once. In local mode,
unowned sessions are assigned to `local`. Idempotent, cheap (one scan).

**Discord.** `discord/mod.rs::session_for` creates the session with owner
`discord:<author_id>` (the `key` already ends in the author id when
`group_sessions_per_user` is on; for a shared channel use the channel key
itself as the owner, `discord:channel:<id>`). `cfg.auth.policy_for_discord()`
returns the `discord_role` policy; `policy_for(user_id)` falls back to it for
any `discord:` prefixed id, so workers resolve it the same way. A `[[users]]`
entry may carry `discord_id = "..."` to bind a Discord identity to a real
account, in which case the owner is that user's id and the sidebar shows those
conversations to them. That binding is a phase-4 nicety.

### 3.9 Agent-core changes (small, and all soft)

`agents/agent-core/src/tools.rs`:

```rust
/// Tools the user's policy withholds by name. Soft: the host enforces
/// capability families and component tools itself; this exists so a withheld
/// built-in is not offered and, if named anyway, is refused with a reason the
/// model can read rather than a trap it cannot.
fn policy_denies(name: &str) -> bool {
    sys::config_get("policy_deny_tools")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => pat == name,
        })
}
```

- `available(mode)`: after the existing `if read_only(mode) { retain }`, add
  `tools.retain(|t| !policy_denies(t.name))`.
- `definitions_for` and `manifests`: skip a component whose name
  `policy_denies`.
- `invoke`: before the mode check, `if policy_denies(name) { return Err(..) }`.
- `groups::route_once` / `admit`: drop ids in `policy_deny_groups` (read the
  same way); `repair_pin` must not force a denied always-on group back in
  (`core` is never deniable: validation refuses `deny_groups` containing
  `core`).

`Turn::new` already prefers the session's model and falls back to
`config_get("model")`, which now answers the user's default. `read_only(mode)`
already reads `list_modes`, which the host marks. No other agent change.

`groups.rs::publish_table` writes to `"global"` from an agent store; unchanged
and unaffected by the gateway-only global-write rule.

### 3.10 Gateway guest changes (small)

`gateways/gateway-web/src/handlers.rs`:

- `USER_AVATAR_KEY` and `MODEL_KEY` reads/writes use scope `"user"` instead of
  `"global"`. The model overlay becomes per-user; a global overlay is not
  needed because `[[models]]` plus the role list is the shared catalogue.
- `save_model` keeps accepting any slug (the host refuses at
  `set_session_model` when the policy has a list; with no list the user may add
  models as today). The `models` frame gains `"restricted": bool` from
  `sys::config_get("policy_models_restricted")` so the inspector can say "your
  role fixes the catalogue" and disable the add form.
- Nothing else. `sessions()`, `open`, `history`, `catalog` are unchanged and
  become per-user through the host.

UI (`gateways/gateway-web/src/ui`):

- `app.js`: handle the host's `user` frame: `store.set({ user })`; footer
  shows the display name with a `Log out` control that POSTs `/logout` via a
  hidden form (no JS fetch needed; a form keeps it working without the
  socket); hide the `admin` link unless `user.admin`; hide the archive/new
  buttons for nobody (readers still own conversations).
- `lib/socket.js`: after three consecutive `onclose` without ever opening,
  `fetch("/api/me")`; on 401, `location.assign("/login?next=" + path)`. This
  is what turns an expired cookie into a login page instead of an endless
  "reconnecting…".
- `views/panel.js` tools tab: a tool whose capability is withheld simply is not
  in `available_tools`; add one line under the list, "some tools are withheld
  by your role", when `user.read_only` or the `tools` frame's count is below
  the group table's member count. Cosmetic; do last.
- `index.html`: a `<form id="logout" method="post" action="/logout" hidden>`.

The login page is host-rendered in `auth.rs` (see 4.4), styled like `/admin`.

### 3.11 What stays deliberately out of scope

- Password reset, email, OAuth, TOTP. The account model is deliberately the
  minimum; the `Principal` abstraction is where an OAuth resolver would plug in.
- Sharing a conversation between users. `OWNERS` is one owner per session;
  `see_all_sessions` for admins is the only cross-user read.
- Per-user filesystem roots or per-user workspaces. The workspace stays
  shared; a role may lose write or all access to it.
- Trusting reverse-proxy headers. `public_origin` is enough for a proxy that
  forwards `Host` unchanged, which is the common configuration.
- Admin UI for editing users. Users are config. A later phase may add a
  db-backed `USERS` table and `/admin/users`; the `Principal` resolver already
  hides where a user came from.

---

## 4. Code

Sketches, not drop-ins: names and signatures are chosen to fit the existing
modules, and the implementer should expect to adjust imports and error
plumbing. Each block names the file it belongs in.

### 4.1 `crates/thetis/src/policy.rs`

```rust
//! Per-user authorization: what a principal may pick, reach and change.
//!
//! Resolved once at startup from `[auth]`, `[[roles]]` and `[[users]]` into an
//! `EffectivePolicy` per user, then read on every host import. Three layers,
//! each narrowing the last: the global configuration is the universe, a role
//! restricts it, a user's overrides restate the role key by key.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    FilesystemRead, FilesystemWrite, FilesystemDelete,
    Terminal, Ssh, Devkit, Control, ConfigWrite, BranchWrite,
    Delegation, SkillsWrite, Transcripts, ComponentTools, Sandbox,
    Workspace, WorkspaceWrite,
}

impl Cap {
    pub fn parse(s: &str) -> Option<Cap> {
        serde_json::from_value(serde_json::Value::String(s.trim().to_string())).ok()
    }
    pub fn all() -> &'static [Cap] { &[ /* every variant, for /api/me */ ] }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectivePolicy {
    pub admin: bool,
    pub read_only: bool,
    pub denied: BTreeSet<Cap>,
    pub models: Vec<String>,
    pub default_model: String,
    pub modes: Vec<String>,
    pub default_mode: String,
    pub deny_tools: Vec<String>,
    pub deny_groups: Vec<String>,
    pub spend_limit_usd: f64,
    pub max_children: usize,
    pub see_all_sessions: bool,
    /// Whether `models` was narrowed by a role or override. The catalogue
    /// inspector uses it to explain why the add form is disabled.
    pub models_restricted: bool,
}

impl EffectivePolicy {
    pub fn denies(&self, cap: Cap) -> bool { /* as in 3.2 */ }
    pub fn allows_model(&self, id: &str) -> bool { self.models.iter().any(|m| m == id) }
    pub fn allows_mode(&self, id: &str) -> bool { self.modes.iter().any(|m| m == id) }
    pub fn denies_tool(&self, name: &str) -> bool { /* as in 3.2 */ }
    pub fn denies_group(&self, id: &str) -> bool { self.deny_groups.iter().any(|g| g == id) }

    pub fn unrestricted(models: &[crate::config::ModelSpec], default_model: &str,
                        modes: &[crate::config::ModeSpec], default_mode: &str,
                        max_children: usize) -> Self {
        Self {
            admin: true,
            read_only: false,
            denied: BTreeSet::new(),
            models: models.iter().map(|m| m.id.clone()).collect(),
            default_model: default_model.to_string(),
            modes: modes.iter().map(|m| m.id.clone()).collect(),
            default_mode: default_mode.to_string(),
            deny_tools: Vec::new(),
            deny_groups: Vec::new(),
            spend_limit_usd: 0.0,
            max_children,
            see_all_sessions: false,
            models_restricted: false,
        }
    }
}

/// One layer of narrowing, as written in a `[[roles]]` entry or a
/// `[users.overrides]` table. Every field optional: `None` inherits.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyLayer {
    pub admin: Option<bool>,
    pub read_only: Option<bool>,
    pub deny_capabilities: Option<Vec<String>>,
    pub models: Option<Vec<String>>,
    pub default_model: Option<String>,
    pub modes: Option<Vec<String>>,
    pub default_mode: Option<String>,
    pub deny_tools: Option<Vec<String>>,
    pub deny_groups: Option<Vec<String>>,
    pub spend_limit_usd: Option<f64>,
    pub max_children: Option<usize>,
    pub see_all_sessions: Option<bool>,
}

/// Applies `layers` in order on top of the unrestricted base and validates
/// the result against the catalogues. `who` names the role or user for the
/// error message.
pub fn resolve(base: &EffectivePolicy, layers: &[&PolicyLayer], who: &str,
               all_models: &[String], all_modes: &[String]) -> anyhow::Result<EffectivePolicy> {
    let mut p = base.clone();
    for layer in layers {
        if let Some(v) = layer.admin { p.admin = v; }
        if let Some(v) = layer.read_only { p.read_only = v; }
        if let Some(caps) = &layer.deny_capabilities {
            p.denied = caps.iter().map(|c| Cap::parse(c)
                .ok_or_else(|| anyhow::anyhow!("{who}: unknown capability `{c}`")))
                .collect::<anyhow::Result<_>>()?;
        }
        if let Some(models) = &layer.models {
            p.models = models.clone();
            p.models_restricted = true;
        }
        if let Some(m) = &layer.default_model { p.default_model = m.clone(); }
        if let Some(modes) = &layer.modes { p.modes = modes.clone(); }
        if let Some(m) = &layer.default_mode { p.default_mode = m.clone(); }
        if let Some(v) = &layer.deny_tools { p.deny_tools = v.clone(); }
        if let Some(v) = &layer.deny_groups { p.deny_groups = v.clone(); }
        if let Some(v) = layer.spend_limit_usd { p.spend_limit_usd = v; }
        if let Some(v) = layer.max_children { p.max_children = v; }
        if let Some(v) = layer.see_all_sessions { p.see_all_sessions = v; }
    }

    // Catalogue checks. Boot errors, like a profile naming a missing model.
    for m in &p.models {
        anyhow::ensure!(all_models.contains(m), "{who}: model `{m}` is not in [[models]]");
    }
    anyhow::ensure!(!p.models.is_empty(), "{who}: no models would be offered");
    if !p.models.contains(&p.default_model) {
        // The global default may have been narrowed away; fall back loudly.
        tracing::warn!(who, default = %p.default_model, "default model is not in this policy's list; using the first");
        p.default_model = p.models[0].clone();
    }
    for m in &p.modes {
        anyhow::ensure!(all_modes.contains(m), "{who}: mode `{m}` is not in [[modes]]");
    }
    anyhow::ensure!(!p.modes.is_empty(), "{who}: no modes would be offered");
    if !p.modes.contains(&p.default_mode) {
        p.default_mode = p.modes[0].clone();
    }
    anyhow::ensure!(!p.deny_groups.iter().any(|g| g == "core"),
        "{who}: the `core` tool group cannot be denied; it holds tool_search");
    Ok(p)
}
```

Unit tests for this module: inheritance (role narrows, override restates),
read-only implies the write capabilities, unknown capability is an error,
prefix tool patterns, default model falling back into the narrowed list,
`core` cannot be denied.

### 4.2 `crates/thetis/src/config.rs` additions

In `spec`:

```rust
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Auth {
    pub mode: String,                 // "local" | "users"
    pub session_ttl_hours: u64,
    pub claim_unowned: String,
    pub discord_role: String,
    pub lockout_after: u32,
    pub lockout_secs: u64,
}
impl Default for Auth {
    fn default() -> Self {
        Self { mode: "local".into(), session_ttl_hours: 720, claim_unowned: String::new(),
               discord_role: String::new(), lockout_after: 5, lockout_secs: 60 }
    }
}

#[derive(Debug, Deserialize)]
pub struct Role {
    pub id: String,
    #[serde(default)] pub description: String,
    #[serde(flatten)] pub policy: crate::policy::PolicyLayer,
}

/// No `Debug`: holds a password hash. A hash is not a secret in the way a
/// key is, but it does not belong in a log line either.
#[derive(Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)] pub name: String,
    pub role: String,
    #[serde(default)] pub password_hash: String,
    #[serde(default)] pub password_env: String,
    #[serde(default)] pub discord_id: String,
    #[serde(default)] pub overrides: crate::policy::PolicyLayer,
}
```

Add `pub auth: Auth, pub roles: Vec<Role>, pub users: Vec<User>` to
`spec::File`, and to `spec::Server` add `pub public_origin: String`.

Resolved settings:

```rust
#[derive(Clone)]
pub struct UserSpec {
    pub id: String,
    pub name: String,
    pub role: String,
    pub password_hash: Secret,
    pub discord_id: String,
    pub policy: Arc<crate::policy::EffectivePolicy>,
}

#[derive(Clone)]
pub struct AuthSettings {
    pub users_mode: bool,
    pub session_ttl: Duration,
    pub claim_unowned: String,
    pub lockout_after: u32,
    pub lockout: Duration,
    pub users: Vec<UserSpec>,
    pub roles: BTreeMap<String, Arc<crate::policy::EffectivePolicy>>,
    pub discord_policy: Arc<crate::policy::EffectivePolicy>,
    pub local_policy: Arc<crate::policy::EffectivePolicy>,
}

impl AuthSettings {
    pub fn user(&self, id: &str) -> Option<&UserSpec> { self.users.iter().find(|u| u.id == id) }

    /// The policy for a session owner. Unknown ids get the most restrictive
    /// thing that still lets the conversation be read: the discord policy for
    /// a `discord:` owner, otherwise the local policy in local mode and a
    /// read-only reader policy in users mode.
    pub fn policy_for(&self, owner: &str) -> Arc<EffectivePolicy> { ... }
}
```

On `Config`: `pub auth: AuthSettings` and `pub public_origin: Option<Origin>`
(`struct Origin { scheme: String, authority: String }`). Env overrides:
`THETIS_AUTH_MODE`, `THETIS_PUBLIC_ORIGIN`, and `THETIS_USER_<ID>_PASSWORD_HASH`
as a convenience that fills `password_hash` for the user with that id (uppercase
id, non-alphanumerics to `_`).

In `assemble`, after modes and profiles are resolved:

```rust
let all_model_ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
let all_mode_ids: Vec<String> = modes.iter().map(|m| m.id.clone()).collect();
let base = EffectivePolicy::unrestricted(&models, &model, &modes, &default_mode,
                                         file.subagents.max_children);

let mut roles = BTreeMap::new();
for r in &file.roles {
    anyhow::ensure!(!r.id.is_empty(), "every [[roles]] entry needs an id");
    anyhow::ensure!(!roles.contains_key(&r.id), "role `{}` is defined twice", r.id);
    let p = policy::resolve(&base, &[&r.policy], &format!("role `{}`", r.id),
                            &all_model_ids, &all_mode_ids)?;
    roles.insert(r.id.clone(), Arc::new(p));
}

let users_mode = match env.string("THETIS_AUTH_MODE").unwrap_or(file.auth.mode).as_str() {
    "local" => false,
    "users" => true,
    other => anyhow::bail!("auth.mode must be `local` or `users`, not `{other}`"),
};

let mut users = Vec::new();
for u in file.users {
    anyhow::ensure!(valid_user_id(&u.id), "user id `{}` must match [a-z0-9._-]{{1,64}}", u.id);
    anyhow::ensure!(!users.iter().any(|x: &UserSpec| x.id == u.id), "user `{}` is defined twice", u.id);
    let role = roles.get(&u.role)
        .ok_or_else(|| anyhow::anyhow!("user `{}` names role `{}`, which is not in [[roles]]", u.id, u.role))?;
    let hash = resolve_password_hash(env, &u)?;   // exactly one source; env must be set
    let p = policy::resolve(role, &[&u.overrides], &format!("user `{}`", u.id),
                            &all_model_ids, &all_mode_ids)?;
    users.push(UserSpec { id: u.id, name: if u.name.is_empty() { id } else { u.name },
                          role: u.role, password_hash: Secret::new(hash),
                          discord_id: u.discord_id, policy: Arc::new(p) });
}

if users_mode {
    anyhow::ensure!(!users.is_empty(),
        "auth.mode = \"users\" but no [[users]] are configured; run `thetis hash-password` and add one to thetis.local.toml");
    anyhow::ensure!(users.iter().any(|u| u.policy.admin), "auth.mode = \"users\" needs at least one admin user");
    anyhow::ensure!(users.iter().any(|u| u.id == file.auth.claim_unowned),
        "auth.claim_unowned must name a configured user");
    if !bind_addr.ip().is_loopback() {
        anyhow::ensure!(public_origin.is_some(),
            "auth.mode = \"users\" bound off loopback needs server.public_origin");
    }
}
```

`discord_policy`: the role named by `auth.discord_role`, or a built-in
read-only policy (`read_only = true`, modes = `[discord.mode]`, denied
`Transcripts`, `Delegation`, `WorkspaceWrite`) when unset. `local_policy` is
`base` with `admin = true`.

### 4.3 `crates/thetis/src/store.rs` additions

```rust
const OWNERS: TableDefinition<&str, &str> = TableDefinition::new("owners");
const LOGINS: TableDefinition<&str, &[u8]> = TableDefinition::new("logins");
const USER_SPEND: TableDefinition<&str, f64> = TableDefinition::new("user_spend");

impl Store {
    pub fn create_session(&self, title: Option<String>, mode: &str, owner: &str) -> Result<SessionMeta> {
        let meta = SessionMeta { /* as today */ };
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(SESSIONS)?;
            t.insert(meta.id.as_str(), serde_json::to_vec(&meta)?.as_slice())?;
            let mut owners = txn.open_table(OWNERS)?;
            owners.insert(meta.id.as_str(), owner)?;
        }
        txn.commit()?;
        Ok(meta)
    }

    pub fn owner_of(&self, session_id: &str) -> Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(OWNERS)?;
        Ok(t.get(session_id)?.map(|v| v.value().to_string()))
    }

    pub fn owner_of_root(&self, session_id: &str) -> Result<Option<String>> {
        let root = crate::subagents::Subagents::new(self).root_of(session_id);
        self.owner_of(&root)
    }

    pub fn set_owner(&self, session_id: &str, owner: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        { txn.open_table(OWNERS)?.insert(session_id, owner)?; }
        txn.commit()?;
        Ok(())
    }

    /// Top-level conversations owned by `owner` (every one when `None`),
    /// most recently active first. Sub-agents are dropped exactly as
    /// `list_sessions` drops them. One transaction, for the same reason
    /// `sessions_with_subagent_rows` uses one.
    pub fn list_sessions_owned(&self, owner: Option<&str>, include_archived: bool) -> Result<Vec<SessionMeta>> {
        let txn = self.db.begin_read()?;
        let sessions = txn.open_table(SESSIONS)?;
        let children = txn.open_table(SUBAGENTS)?;
        let owners = txn.open_table(OWNERS)?;
        let mut out = Vec::new();
        for row in sessions.iter()? {
            let (id, v) = row?;
            if children.get(id.value())?.is_some() { continue; }
            if let Some(want) = owner {
                let has = owners.get(id.value())?.map(|o| o.value().to_string());
                if has.as_deref() != Some(want) { continue; }
            }
            let meta: SessionMeta = serde_json::from_slice(v.value())?;
            if include_archived || !meta.archived { out.push(meta); }
        }
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    pub fn unowned_sessions(&self) -> Result<Vec<String>> {
        let txn = self.db.begin_read()?;
        let sessions = txn.open_table(SESSIONS)?;
        let children = txn.open_table(SUBAGENTS)?;
        let owners = txn.open_table(OWNERS)?;
        let mut out = Vec::new();
        for row in sessions.iter()? {
            let (id, _) = row?;
            if children.get(id.value())?.is_none() && owners.get(id.value())?.is_none() {
                out.push(id.value().to_string());
            }
        }
        Ok(out)
    }

    /// Every OWNERS row, for a filter that would otherwise do one read per
    /// session.
    pub fn owners_map(&self) -> Result<HashMap<String, String>> { ... }

    // logins ------------------------------------------------------------
    pub fn put_login(&self, token_hash: &str, row: &LoginRow) -> Result<()> { ... }
    pub fn get_login(&self, token_hash: &str) -> Result<Option<LoginRow>> { ... }
    pub fn remove_login(&self, token_hash: &str) -> Result<()> { ... }
    pub fn touch_login(&self, token_hash: &str, now_ms: u64, expires_ms: u64) -> Result<()> { ... }
    pub fn prune_expired_logins(&self, now_ms: u64) -> Result<usize> { ... }

    // user spend ----------------------------------------------------------
    pub fn get_user_spend(&self, user: &str) -> Result<f64> { ... }
    pub fn add_user_spend(&self, user: &str, usd: f64) -> Result<f64> { ... }
}
```

Tests: `create_session` writes an owner; `list_sessions_owned` filters and
still hides sub-agents; `unowned_sessions` finds only legacy rows; login rows
round-trip and prune; user spend accumulates.

### 4.4 `crates/thetis/src/auth.rs`

```rust
//! Accounts and logins for the web UI.
//!
//! Everything here runs in the gateway process and in native code: like
//! `/admin`, the login page has no WebAssembly in its path, so it keeps
//! working when every guest is broken — which is exactly when an operator
//! needs to get in.

use anyhow::{bail, Context, Result};
use argon2::password_hash::{rand_core::{OsRng, RngCore}, PasswordHash, PasswordHasher,
                            PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::config::Config;
use crate::grip::Grip;
use crate::policy::EffectivePolicy;

pub const COOKIE: &str = "thetis_session";

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub policy: Arc<EffectivePolicy>,
}

impl Principal {
    pub fn is_admin(&self) -> bool { self.policy.admin }

    /// The implicit principal of `auth.mode = "local"`.
    pub fn local(cfg: &Config) -> Arc<Self> {
        Arc::new(Self {
            user_id: "local".into(),
            display_name: "local".into(),
            role: "admin".into(),
            policy: cfg.auth.local_policy.clone(),
        })
    }

    pub fn from_user(u: &crate::config::UserSpec) -> Arc<Self> {
        Arc::new(Self {
            user_id: u.id.clone(),
            display_name: u.name.clone(),
            role: u.role.clone(),
            policy: u.policy.clone(),
        })
    }
}

// --- passwords ---------------------------------------------------------------

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing: {e}"))?
        .to_string())
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

// --- tokens ------------------------------------------------------------------

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64url(&bytes)          // hand-rolled: 44 lines, or add the `base64` crate
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == COOKIE).then(|| v.trim().to_string())
        })
        .next()
}

pub fn set_cookie(cfg: &Config, token: &str, max_age_secs: u64) -> String {
    let secure = cfg.public_origin.as_ref().is_some_and(|o| o.scheme == "https");
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{}",
            if secure { "; Secure" } else { "" })
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

// --- resolution --------------------------------------------------------------

/// Who this request is for, or `None` when it is not logged in.
pub async fn resolve(grip: &Arc<Grip>, headers: &HeaderMap) -> Option<Arc<Principal>> {
    let cfg = &grip.cfg;
    if !cfg.auth.users_mode {
        return Some(Principal::local(cfg));
    }
    let token = cookie_value(headers)?;
    let hash = token_hash(&token);
    let store = grip.local_store()?;
    let row = store.get_login(&hash).ok().flatten()?;
    let now = crate::store::now_ms();
    if row.expires_ms <= now {
        let _ = store.remove_login(&hash);
        return None;
    }
    let Some(user) = cfg.auth.user(&row.user_id) else {
        // The account was removed from config; its logins go with it.
        let _ = store.remove_login(&hash);
        return None;
    };
    // Sliding expiry, written at most once a minute per login.
    if now.saturating_sub(row.last_seen_ms) > 60_000 {
        let _ = store.touch_login(&hash, now, now + cfg.auth.session_ttl.as_millis() as u64);
    }
    Some(Principal::from_user(user))
}

/// Whether a principal may read a session: its owner, or an admin allowed to
/// see everything. Sub-agents resolve to their root.
pub fn may_access(grip: &Grip, who: &Principal, session_id: &str) -> Result<()> {
    if who.policy.see_all_sessions {
        return Ok(());
    }
    let store = grip.local_store().context("ownership is a gateway concern")?;
    match store.owner_of_root(session_id)? {
        Some(owner) if owner == who.user_id => Ok(()),
        Some(_) => bail!("that conversation belongs to someone else"),
        // Unowned means never claimed, which after the boot claim means it
        // does not exist. Refuse rather than guess.
        None => bail!("no such conversation"),
    }
}

// --- login flow --------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct LoginForm {
    pub user: String,
    pub password: String,
    #[serde(default)]
    pub next: String,
}

pub async fn login(grip: &Arc<Grip>, form: LoginForm, user_agent: &str) -> Response {
    let cfg = &grip.cfg;
    if !cfg.auth.users_mode {
        return Redirect::to("/").into_response();
    }
    if lockout::is_locked(&form.user, cfg) {
        return page(cfg, Some("Too many attempts. Try again in a minute."), &form.next)
            .into_response();
    }
    let ok = cfg.auth.user(&form.user)
        .map(|u| verify_password(&form.password, u.password_hash.expose()))
        .unwrap_or_else(|| {
            // Same cost for an unknown user as for a wrong password, so the
            // response time does not say which accounts exist.
            let _ = verify_password(&form.password, DUMMY_HASH);
            false
        });
    if !ok {
        lockout::record_failure(&form.user, cfg);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return page(cfg, Some("Wrong user or password."), &form.next).into_response();
    }
    lockout::clear(&form.user);

    let token = new_token();
    let now = crate::store::now_ms();
    let ttl_ms = cfg.auth.session_ttl.as_millis() as u64;
    let row = crate::store::LoginRow {
        user_id: form.user.clone(),
        created_ms: now,
        last_seen_ms: now,
        expires_ms: now + ttl_ms,
        user_agent: user_agent.chars().take(200).collect(),
    };
    if let Some(store) = grip.local_store() {
        if let Err(e) = store.put_login(&token_hash(&token), &row) {
            tracing::error!(error = %e, "could not record a login");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not record the login").into_response();
        }
    }
    tracing::info!(user = %form.user, "logged in");
    let next = safe_next(&form.next);
    (
        [(header::SET_COOKIE, set_cookie(cfg, &token, cfg.auth.session_ttl.as_secs()))],
        Redirect::to(&next),
    ).into_response()
}

pub async fn logout(grip: &Arc<Grip>, headers: &HeaderMap) -> Response {
    if let (Some(token), Some(store)) = (cookie_value(headers), grip.local_store()) {
        let _ = store.remove_login(&token_hash(&token));
    }
    ([(header::SET_COOKIE, clear_cookie())], Redirect::to("/login")).into_response()
}

/// Only a same-origin path is honoured as a post-login destination, so the
/// login form cannot be used as an open redirect.
fn safe_next(raw: &str) -> String {
    if raw.starts_with('/') && !raw.starts_with("//") && !raw.contains("://") {
        raw.to_string()
    } else {
        "/".to_string()
    }
}

/// The host-rendered login page. Deliberately plain: no script, no socket,
/// no guest. Same visual language as /admin.
pub fn page(cfg: &Config, message: Option<&str>, next: &str) -> axum::response::Html<String> {
    let banner = message.map(|m| format!(r#"<p class="banner bad">{}</p>"#, html_escape(m)))
        .unwrap_or_default();
    axum::response::Html(format!(r#"<!doctype html><meta charset="utf-8"><title>{name} — sign in</title>
<style>body{{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;max-width:24rem;margin:6rem auto;padding:0 1.5rem;color:#e6e6e6;background:#16161a}}
label{{display:block;margin:.8rem 0 .2rem;color:#9a9aa8;font-size:.8rem;text-transform:uppercase;letter-spacing:.06em}}
input{{font:inherit;width:100%;box-sizing:border-box;padding:.5rem .7rem;border-radius:6px;border:1px solid #35353f;background:#26262c;color:#e6e6e6}}
button{{font:inherit;margin-top:1.2rem;padding:.5rem 1rem;border-radius:6px;border:1px solid #35353f;background:#31313c;color:#e6e6e6;cursor:pointer}}
.banner{{padding:.7rem 1rem;border-radius:8px;margin:1rem 0}} .banner.bad{{background:#2f1d21;border:1px solid #5a2f38}}</style>
<h1>{name}</h1>
{banner}
<form method="post" action="/login">
  <input type="hidden" name="next" value="{next}">
  <label for="user">User</label><input id="user" name="user" autocomplete="username" autofocus required>
  <label for="password">Password</label><input id="password" name="password" type="password" autocomplete="current-password" required>
  <button>Sign in</button>
</form>"#,
        name = html_escape(&cfg.agent_name),
        banner = banner,
        next = html_escape(next)))
}

/// A real argon2 hash of a throwaway password, so an unknown user costs the
/// same to reject as a known one.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$...generate once, paste here...";

mod lockout {
    // Mutex<HashMap<String, (u32 failures, Instant first_failure)>> in a
    // OnceLock; is_locked / record_failure / clear. Cleared entries expire
    // after cfg.auth.lockout.
}
```

`main.rs` gains:

```rust
Some("hash-password") => {
    let password = read_password_from_tty_or_stdin()?;   // rpassword is not a dep; read stdin
    println!("{}", thetis::auth::hash_password(&password)?);
    return Ok(());
}
```

Reading without echo needs a terminal ioctl; `libc` is already a dependency,
so a 15-line `tcsetattr` toggle is enough, with `--stdin` for scripts.

### 4.5 `crates/thetis/src/web.rs` changes

Router:

```rust
let app = Router::new()
    .route("/ws", get(ws_upgrade))
    .route("/login", get(login_page).post(login_submit))
    .route("/logout", post(logout))
    .route("/api/me", get(whoami))
    .route("/admin", get(admin_page))
    ... // unchanged routes
    .layer(middleware::from_fn_with_state(grip.clone(), authenticate))
    .layer(middleware::from_fn_with_state(grip.clone(), guard_origin))
    .with_state(grip.clone());
```

(Layers apply outermost-last, so `guard_origin` runs first.)

```rust
/// Resolves the principal and refuses what has none. `/login` and `/logout`
/// are the only routes that serve an anonymous caller.
async fn authenticate(State(grip): State<Arc<Grip>>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let public = path == "/login" || path == "/logout";
    match crate::auth::resolve(&grip, req.headers()).await {
        Some(principal) => {
            if path.starts_with("/admin") && !principal.is_admin() {
                return (StatusCode::FORBIDDEN, "admin only").into_response();
            }
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        None if public => next.run(req).await,
        None => {
            let wants_html = req.method() == axum::http::Method::GET
                && req.headers().get(header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|a| a.contains("text/html"));
            if wants_html {
                let next = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
                Redirect::to(&format!("/login?next={}", urlencode(next))).into_response()
            } else {
                (StatusCode::UNAUTHORIZED, "sign in first").into_response()
            }
        }
    }
}
```

`ws_upgrade` reads `Extension(principal): Extension<Arc<Principal>>` and passes
it into `connection`. Inside `connection`:

- send the `user` frame first (`out_tx.send(user_frame(&principal))`);
- pass `&principal` into `debug_api::handle`, `system_api::handle`,
  `branch_api::handle`, `workspace_api::handle`, and
  `gateway::on_client_message`;
- `GatewayAction::Subscribe(id)`: `match auth::may_access(&grip, &principal, &id)`,
  insert on `Ok`, otherwise send `error_frame("that conversation is not yours", Some("open"))`.

`workspace_download` / `workspace_upload` take the `Extension` and `require`
`Workspace` / `WorkspaceWrite` before resolving the path. `preview_response`
calls `may_access`. `admin_*` handlers need nothing: the middleware already
gated `/admin`.

`user_frame`:

```rust
fn user_frame(p: &Principal) -> String {
    serde_json::json!({
        "type": "user",
        "id": p.user_id, "name": p.display_name, "role": p.role,
        "admin": p.is_admin(),
        "read_only": p.policy.read_only,
        "see_all": p.policy.see_all_sessions,
        "workspace": if p.policy.denies(Cap::Workspace) { "none" }
                     else if p.policy.denies(Cap::WorkspaceWrite) { "read" } else { "write" },
        "denied": p.policy.denied,
    }).to_string()
}
```

### 4.6 `crates/thetis/src/host_api.rs` changes

The scoping helper grows a second arm:

```rust
impl HostState {
    /// Whether this call may touch `session_id`.
    ///
    /// An agent or tool store is pinned to one session (`scope_ok`, as
    /// before). A gateway store is pinned to a *person*: it may touch any
    /// session that person owns, and nothing else. A store with neither — a
    /// probe, a describe — may touch nothing that names a session.
    fn may_access(&self, session_id: &str) -> Result<()> {
        if self.session_id.is_some() {
            return self.scope_ok(session_id);
        }
        match &self.principal {
            Some(p) => crate::auth::may_access(self.grip(), p, session_id).wt(),
            None if !self.grip().cfg.auth.users_mode => Ok(()),   // local mode probes
            None => Err(err("this call has no user")),
        }
    }

    fn require(&self, cap: Cap) -> Result<()> { /* 3.7 */ }

    /// Which user's `user:` scope this store reads.
    async fn user_scope(&self) -> Result<String> {
        if let Some(p) = &self.principal {
            return Ok(format!("user:{}", p.user_id));
        }
        if let Some(sid) = &self.session_id {
            if let Some(owner) = self.grip().persist.owner_of_root(sid).await.wt()? {
                return Ok(format!("user:{owner}"));
            }
        }
        Err(err("no user for this call"))
    }
}
```

`sys::kv_get` / `kv_put`:

```rust
let scope = match scope.as_str() {
    "global" => {
        if writing && self.session_id.is_none()
            && self.principal.as_ref().is_some_and(|p| !p.is_admin()) {
            return Err(err("only an administrator may write global settings"));
        }
        scope
    }
    "user" => self.user_scope().await?,
    s if s.starts_with("user:") => return Err(err("user scopes are addressed as `user`")),
    _ => { self.may_access(&scope)?; scope }
};
```

Every `session::*` import that called `scope_ok` calls `may_access`.
`list_sessions`:

```rust
async fn list_sessions(&mut self, include_archived: bool) -> Result<Vec<SessionMeta>> {
    self.budget.entered_host("list_sessions");
    let owner = self.list_owner().await.wt()?;    // None = all, Some(id) = theirs
    self.grip().persist.list_sessions_owned(owner.as_deref(), include_archived).await.wt()
}
```

where `list_owner` is the principal's id (or `None` with `see_all_sessions`),
the session owner for an agent store, and `None` for a local-mode probe.

`create_session`:

```rust
let owner = self.principal.as_ref().map(|p| p.user_id.clone())
    .unwrap_or_else(|| "local".to_string());
self.grip().persist.create_session(title, &mode, &owner).await.map(|s| s.id).wt()
```

with `mode = self.policy.default_mode.clone()`.

`llm::stream_open` / `chat`, before opening:

```rust
if let Some(model) = requested_model(&request_json) {
    if !self.policy.allows_model(&model) {
        return Ok(Err(LlmError::BadRequest(format!(
            "model `{model}` is not available to this user; pick one of: {}",
            self.policy.models.join(", ")))));
    }
}
```

`check_budget` grows a second limit: `policy.spend_limit_usd` against
`persist.get_user_spend(owner)`, and `record_usage` adds to both tables.

`tooling::invoke`:

```rust
self.require(Cap::ComponentTools)?;
if self.policy.denies_tool(&name) {
    return Ok(Err(format!("'{name}' is withheld for this user by policy")));
}
if self.policy.read_only && !self.grip().tool_registry().iter()
    .any(|m| m.name == name && m.capabilities.iter().any(|c| c == "read-only")) {
    return Ok(Err(format!("'{name}' may change something, and this user is read-only")));
}
```

The rest of the matrix in 3.7 is one `self.require(...)?` per import.

### 4.7 `crates/thetis/src/gateway.rs` changes

`fresh_instance` / `instance_of` take `principal: Arc<Principal>` and use
`grip.gateway_store(budget, principal)`. `serve_asset`, `serve_preview_asset`
and `on_client_message` take `&Arc<Principal>`. `Renderer` is unchanged.

### 4.8 `crates/thetis/src/persist.rs` changes

Signature changes and new arms as listed in 3.8. The `delegate!` additions:

```rust
pub async fn create_session(&self, title: Option<String>, mode: &str, owner: &str) -> Result<SessionMeta>
pub async fn list_sessions_owned(&self, owner: Option<&str>, include_archived: bool) -> Result<Vec<SessionMeta>>
pub async fn owner_of_root(&self, session_id: &str) -> Result<Option<String>>
pub async fn session_policy(&self, session_id: &str) -> Result<EffectivePolicy>
pub async fn get_user_spend(&self, user: &str) -> Result<f64>
pub async fn add_user_spend(&self, user: &str, usd: f64) -> Result<f64>
```

`serve_store_call_inner` gets `cfg: &Config` (for `session_policy`) and the
`caller_owner` derivation from 3.8. Keep `list_sessions` as an IPC method name
for compatibility with a branch worker on an older kernel, but serve it through
`list_sessions_owned(caller_owner, ..)`.

Add to the `local_and_remote_agree` test: a session created through the remote
arm carries the caller's owner; a remote `list_sessions` from a worker pinned to
user A's session does not list user B's; `search_transcripts` from that worker
finds nothing in B's log.

### 4.9 `agents/agent-core` and the gateway guest

As in 3.9 and 3.10. The guest changes are two constants' scopes and one new
field in the `catalog` frame; the agent changes are `policy_denies` and its
three call sites plus the group-deny filter.

Both guests are built for `wasm32-wasip2` by the orchestrator; `cargo test` in
each crate directory runs the pure tests natively (`target-native` exists for
this). Add a test for `policy_denies` pattern matching next to the existing
`interrupt_tests`.

### 4.10 UI

`gateways/gateway-web/src/ui/lib/socket.js`:

```js
this.socket.onclose = () => {
  this.onStatus("offline", "reconnecting…");
  this.failures = (this.failures || 0) + 1;
  if (this.failures >= 3 && !this.everOpened) {
    // Three refusals with no open in between is a rejected upgrade, and the
    // usual reason is a login that expired. Ask, and go to the door if so.
    fetch("/api/me", { credentials: "same-origin" }).then((r) => {
      if (r.status === 401) location.assign(`/login?next=${encodeURIComponent(location.pathname)}`);
    }).catch(() => {});
  }
  setTimeout(() => this.connect(), this.retryDelay);
  this.retryDelay = Math.min(this.retryDelay * 2, 8000);
};
this.socket.onopen = () => { this.everOpened = true; this.failures = 0; ... };
```

`app.js`:

```js
.on("user", (frame) => {
  store.set({ user: frame });
  const who = $("user-name");
  if (who) who.textContent = frame.name || frame.id;
  setHidden($("admin-link"), !frame.admin);
  setHidden($("logout"), frame.id === "local");
})
```

`index.html` footer: `<span id="user-name" class="quiet"></span>`, give the
admin anchor `id="admin-link"`, and add
`<form id="logout" method="post" action="/logout" hidden><button class="quiet-link">log out</button></form>`.

---

## 5. Phases

Each phase leaves trunk shippable. Estimates assume one implementer familiar
with the repo; the first phase is the largest because it touches the plumbing
every later phase rides on.

### Phase 0: ownership plumbing, no visible change (about a day)

1. `Cargo.toml`: add `argon2 = "0.5"`.
2. `store.rs`: `OWNERS`, `LOGINS`, `USER_SPEND` tables; `create_session` owner
   parameter; `owner_of`, `owner_of_root`, `set_owner`, `list_sessions_owned`,
   `unowned_sessions`, `owners_map`; tests.
3. Update every `create_session` caller to pass `"local"` (Discord passes
   `discord:<id>`).
4. `roles/gateway.rs`: boot claim of unowned sessions to `local`.
5. `persist.rs`: new methods and IPC arms; `caller_owner` derivation;
   transcript arms take the owner filter (still `None` everywhere).
6. `policy.rs` with `EffectivePolicy`, `PolicyLayer`, `resolve`, tests.
7. `config.rs`: `[auth]`, `[[roles]]`, `[[users]]`, `server.public_origin`;
   `AuthSettings` with `local_policy`; validation; tests for layering and
   every boot error above.
8. `runtime.rs`: `principal` and `policy` fields on `HostState`;
   `Grip::gateway_store` / `session_store` / `policy_for_session`;
   `run_turn`, `agent_tools`, `invoke_tool` use `session_store`.
9. `auth.rs` with `Principal::local`, hashing, tokens, cookie helpers, `resolve`
   (local mode only returns `local`), `may_access`; `main.rs hash-password`.
10. `gateway.rs` / `web.rs`: thread the principal through (always `local`).
11. `cargo test -p thetis` green; run the live smoke (`ws_live.rs`) against a
    scratch instance.

### Phase 1: users mode (about two days)

1. `web.rs`: `guard_origin`, `authenticate`, `/login`, `/logout`, `/api/me`,
   `user` frame, subscribe check, principal into every host-side frame handler,
   preview and workspace route checks.
2. `host_api.rs`: `may_access` in every `session::*` import; `list_sessions`
   by owner; `create_session` stamps owner; `kv` scope `user` and the global
   write rule.
3. `persist.rs`: owner filtering live in `list_sessions`, `get_session`, and
   the four transcript arms.
4. Boot claim honours `claim_unowned` in users mode.
5. `handlers.rs`: avatar and model overlay to scope `user`.
6. UI: footer badge, logout form, admin link gating, socket 401 redirect.
7. Docs: `thetis.toml` section, README "Configuration" table and the
   "Not yet built" line, `SETUP.md` section on users mode and running behind a
   proxy, `skills/thetis-internals/config-and-recovery/SKILL.md`.
8. Tests: `auth.rs` unit tests (hash round-trip, cookie parsing, `safe_next`,
   expiry, unknown user removes the login); `store` login tests; a new
   `tests/ws_auth.rs` live test: log in as two users over HTTP, open two
   sockets with the cookies, assert `sessions` lists differ, assert `open` on
   the other's id yields an `error` frame, assert `/admin` is 403 for the
   non-admin, assert `/ws` without a cookie is 401.

### Phase 2: policy enforcement (about two days)

1. `host_api.rs`: `require` at every import in the matrix; `list_models` /
   `list_modes` filtering and read-only marking; `config_get` keys; model check
   in `llm::*`; component tool checks in `tooling::invoke`; delegation limit.
2. `web.rs`: `require` on workspace frames and routes, `BranchWrite` on branch
   mutations.
3. Worker: `store.session_policy` arm, `persist.session_policy`, the grip cache,
   `policy_for_session` in worker role.
4. `agent-core/tools.rs` and `groups.rs`: `policy_denies`, group denies.
5. `handlers.rs`: `restricted` flag in the catalogue frame; the inspector
   disables the add form when set.
6. Discord: `discord_role` policy, `discord:` owners, `policy_for` fallback.
7. Tests: host_api tests through the existing test-grip pattern
   (`persist.rs:660-748` shows how to stand a store and a peer up): a reader's
   agent store gets `hostfs.available() == false` for write, `list_modes` all
   read-only, `stream_open` refuses a disallowed model; a developer with
   `deny_capabilities = ["control"]` traps on `control.restart` and sees
   `control.available() == false`. Extend `ws_auth.rs`: the reader's `tools`
   frame carries no mutating built-in; `set-model` to a disallowed slug yields
   an `error` frame and the `settings` frame keeps the old value.

### Phase 3: spend, admin visibility, polish (about a day)

1. `USER_SPEND` accounting and the per-user limit in `check_budget`.
2. `see_all_sessions` for admins, with a sidebar toggle ("everyone's
   conversations") that sends `{type:"list", all:true}`; the guest forwards
   the flag as a new `include_all` argument only if a WIT change is accepted;
   otherwise a host-side `list` interception. Prefer the host interception:
   handle `list` with `all: true` in `web.rs` like the other host frames and
   reply with the same `sessions` shape.
3. `/admin`: a users table (id, role, active logins, spend), a "sign out
   everywhere" button per user (`remove_logins_for`), and the boot-claim
   count.
4. A periodic `prune_expired_logins` in the gateway (hourly).
5. `discord_id` binding on `[[users]]`.

---

## 6. Test plan summary

| Layer | Where | What |
|---|---|---|
| Policy resolution | `policy.rs` tests | layering, read-only implications, catalogue validation, `core` undeniable, prefix patterns |
| Config | `config.rs` tests (use `Env::None`) | each boot error; local mode defaults; env overrides for auth mode and hashes |
| Store | `store.rs` tests | owners, filtered listing, unowned scan, logins, user spend |
| Auth | `auth.rs` tests | hash/verify, cookie parse, `safe_next`, expiry and removal on unknown user |
| IPC scoping | `persist.rs` tests | owner-filtered list/get/transcripts across the wire; `create_session` inherits the caller's owner |
| Host imports | new `host_api` tests using the test grip | `may_access`, `require`, model/mode filtering, `user` scope mapping, global write rule |
| Web | `web.rs` unit tests | `guard_origin` with and without `public_origin`; the existing `guard_tests` unchanged; `authenticate` redirect vs 401 |
| Live | `tests/ws_auth.rs` (ignored, like the others) | two users, isolation, admin gating, policy visible in frames |
| Guests | `agents/agent-core` and `gateways/gateway-web` native tests | `policy_denies`; `catalog` frame shape |

---

## 7. Risks and decisions for the owner

1. **Non-loopback exposure.** Users mode is the first time Thetis is meant to
   be reachable from another machine. The kernel still speaks plain HTTP. The
   plan assumes a reverse proxy for TLS and requires `public_origin`; it does
   not add rate limiting beyond the login lockout, nor request body limits
   beyond what exists. An operator exposing it directly to the internet should
   be told, in `SETUP.md`, that this is not the intended deployment.
2. **Built-in tool denials are soft.** A user denied `terminal_run` but allowed
   `terminal_read` is enforced only by agent-core, which the agent can rewrite
   in its own branch. The hard denial is `deny_capabilities = ["terminal"]`.
   Say this in `thetis.toml` next to `deny_tools`.
3. **Shared workspace.** All users share `workspace/`. A reader can be denied
   it entirely; two developers can see each other's files there. Per-user
   workspaces are a bigger change (WASI preopens per turn) and are out of
   scope.
4. **Legacy conversations.** Everything created before this lands is owned by
   `local` (local mode) or by `claim_unowned` (users mode). If those should be
   split among several people, a one-off `thetis chown <session> <user>`
   subcommand is cheap to add; it is not in the plan.
5. **The gateway guest keeps `create_session`.** A user can create as many
   conversations as they like; each costs a branch and a worktree at first
   message. A per-user cap on live workers is a natural follow-on to
   `spend_limit_usd`.
6. **WIT unchanged.** This plan needs no contract change, which is why it can
   land without rebuilding every guest at once. If a `whoami` import is wanted
   later, it goes through `world host-staging` per `bindings.rs:20-30`.
7. **Admin sees own sessions by default.** `see_all_sessions` is opt-in even
   for admins. The alternative (admins see everything always) makes the
   sidebar useless on a busy installation.

---

## 8. Implementer's checklist

- [ ] `argon2` added; `cargo build --release -p thetis` clean.
- [ ] `store.rs` tables and methods, with tests.
- [ ] Every `create_session` call site passes an owner.
- [ ] `policy.rs` and `config.rs` additions, with the boot errors tested.
- [ ] `HostState.principal` / `.policy`; `gateway_store` / `session_store`.
- [ ] `auth.rs`; `thetis hash-password`.
- [ ] `web.rs`: `guard_origin`, `authenticate`, login routes, `user` frame,
      subscribe check, principal into every host-side frame handler.
- [ ] `host_api.rs`: `may_access` everywhere `scope_ok` was; `require` per the
      matrix; `list_models` / `list_modes` / `config_get` policy-aware; `user`
      KV scope; global write rule; model check in `llm::*`.
- [ ] `persist.rs`: `caller_owner`, filtered arms, `session_policy`, user spend.
- [ ] `transcripts.rs`: owner filter, write-path test still passing.
- [ ] Worker: policy fetch and cache.
- [ ] `handlers.rs`: `user` scope for avatar and overlay; `restricted` flag.
- [ ] `agent-core`: `policy_denies`; group denies.
- [ ] UI: footer, logout, admin link, 401 redirect.
- [ ] Discord: owners and role.
- [ ] Docs: `thetis.toml`, README, SETUP.md, `thetis-internals` skill.
- [ ] `tests/ws_auth.rs` passes against a scratch instance in users mode.
