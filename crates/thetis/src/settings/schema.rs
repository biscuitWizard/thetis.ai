//! What every setting is, in one table.
//!
//! `FIELDS` names each scalar in `thetis.toml` with its type, the section it
//! belongs to, a line of help, the environment variable that overrides it, and
//! whether a change needs a restart. `TABLES` does the same for the lists of
//! tables — models, modes, providers, roles, users, sub-agent profiles — as a
//! set of columns. Defaults are not repeated here: they come from
//! `Config::default_file_toml`, so a default changed in `config.rs` is changed
//! for the control panel too. A test checks that every key that document
//! contains has a row here, which is how a new setting cannot be added to the
//! configuration without also being described.
//!
//! Adding a setting is one row. Adding a list section is one `TableSection`.

/// How a value should be edited and parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int,
    Float,
    Text,
    /// Multi-line text: a prompt, a description.
    LongText,
    /// A list of strings.
    List,
    /// Write-only: shown as set or unset, never read back.
    Secret,
    /// A path relative to the project root, or absolute.
    Path,
    Url,
    /// The id of a configured model.
    ModelId,
    /// The id of a configured mode.
    ModeId,
    /// The id of a configured role.
    RoleId,
    /// The id of a configured provider.
    ProviderId,
    /// A map of string to string, edited as `key = value` lines.
    Map,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::Text => "text",
            Kind::LongText => "longtext",
            Kind::List => "list",
            Kind::Secret => "secret",
            Kind::Path => "path",
            Kind::Url => "url",
            Kind::ModelId => "model",
            Kind::ModeId => "mode",
            Kind::RoleId => "role",
            Kind::ProviderId => "provider",
            Kind::Map => "map",
        }
    }
}

/// Where a select's options come from, for kinds that are chosen rather
/// than typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choices {
    None,
    Models,
    Modes,
    Roles,
    Providers,
    Static(&'static [&'static str]),
}

/// Whether the running process would pick a change up.
///
/// Everything is `Required` today: configuration is read once at boot. The
/// variant exists so a setting that becomes hot-reloadable can say so without
/// the surfaces changing shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Required,
    Live,
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// Dotted path, e.g. `llm.model`.
    pub key: &'static str,
    pub kind: Kind,
    /// The top-level section, which is also how the panel groups it.
    pub section: &'static str,
    pub help: &'static str,
    /// The environment variable that overrides it, if one does.
    pub env: Option<&'static str>,
    pub restart: Restart,
    pub choices: Choices,
}

const fn f(key: &'static str, kind: Kind, section: &'static str, help: &'static str) -> Field {
    Field { key, kind, section, help, env: None, restart: Restart::Required, choices: Choices::None }
}
const fn fe(
    key: &'static str,
    kind: Kind,
    section: &'static str,
    help: &'static str,
    env: &'static str,
) -> Field {
    Field { key, kind, section, help, env: Some(env), restart: Restart::Required, choices: Choices::None }
}
const fn fc(
    key: &'static str,
    kind: Kind,
    section: &'static str,
    help: &'static str,
    env: Option<&'static str>,
    choices: Choices,
) -> Field {
    Field { key, kind, section, help, env, restart: Restart::Required, choices }
}

pub const FIELDS: &[Field] = &[
    // --- server -------------------------------------------------------------
    fe("server.bind", Kind::Text, "server", "Address and port the HTTP server listens on. Off loopback needs auth.mode = \"users\" and a public origin.", "THETIS_BIND"),
    fe("server.primary_gateway", Kind::Text, "server", "Which gateway component serves the browser UI: the crate at <paths.gateways>/<gateway_prefix><name>.", "THETIS_GATEWAY"),
    fe("server.admin_enabled", Kind::Bool, "server", "The host-rendered /admin recovery console and this control panel. Keep it on: it is the control surface that still works when every guest is broken.", "THETIS_ADMIN"),
    fe("server.public_origin", Kind::Url, "server", "scheme://authority the UI is reached at through a reverse proxy. Required when bind is not loopback in users mode; https here marks the login cookie Secure.", "THETIS_PUBLIC_ORIGIN"),
    // --- auth ---------------------------------------------------------------
    fc("auth.mode", Kind::Text, "auth", "\"local\" is one implicit administrator on loopback; \"users\" requires roles and users with password hashes.", Some("THETIS_AUTH_MODE"), Choices::Static(&["local", "users"])),
    f("auth.session_ttl_hours", Kind::Int, "auth", "How long a login cookie lives. Sliding: activity extends it."),
    f("auth.claim_unowned", Kind::Text, "auth", "The user who takes ownership of conversations that predate accounts. Must name a configured user in users mode."),
    fc("auth.discord_role", Kind::RoleId, "auth", "The role Discord conversations run under. Empty means a hard read-only policy.", None, Choices::Roles),
    f("auth.lockout_after", Kind::Int, "auth", "Failed sign-ins before an account cools off."),
    f("auth.lockout_secs", Kind::Int, "auth", "How long the cooling-off lasts."),
    // --- paths --------------------------------------------------------------
    fe("paths.data", Kind::Path, "paths", "The database and per-process state. Moving it orphans everything already there.", "THETIS_DATA_DIR"),
    fe("paths.artifacts", Kind::Path, "paths", "Built components and their revisions. Moving it orphans every build on disk.", "THETIS_ARTIFACTS_DIR"),
    f("paths.skills", Kind::Path, "paths", "The skill corpus: one directory per skill."),
    f("paths.templates", Kind::Path, "paths", "Scaffolds the dev kit copies when it creates a tool."),
    f("paths.wit", Kind::Path, "paths", "The contract every guest is compiled against."),
    f("paths.agent", Kind::Path, "paths", "The agent crate."),
    f("paths.gateways", Kind::Path, "paths", "Where gateway crates live."),
    f("paths.gateway_prefix", Kind::Text, "paths", "Prefix a gateway's directory name carries, e.g. gateway-web."),
    f("paths.tools", Kind::Path, "paths", "Where tool crates live; one directory each."),
    f("paths.tool_prefix", Kind::Text, "paths", "Prefix a tool's directory name carries, if any."),
    f("paths.worktrees", Kind::Path, "paths", "Where each conversation's checkout is created."),
    // --- llm ----------------------------------------------------------------
    fe("llm.base_url", Kind::Url, "llm", "The default OpenAI-compatible endpoint, registered as the provider `openrouter`.", "OPENROUTER_BASE_URL"),
    fc("llm.model", Kind::ModelId, "llm", "The model a conversation uses unless it picks another.", Some("THETIS_MODEL"), Choices::Models),
    fe("llm.api_key", Kind::Secret, "llm", "The key for the default endpoint. The environment variable overrides it when set.", "OPENROUTER_API_KEY"),
    fe("llm.request_timeout_secs", Kind::Int, "llm", "How long to wait on a silent server. For a streaming completion this is a read timeout that resets whenever bytes arrive.", "THETIS_REQUEST_TIMEOUT_SECS"),
    fe("llm.max_retries", Kind::Int, "llm", "Retries on a transport or 5xx failure before a turn gives up.", "THETIS_MAX_RETRIES"),
    fc("llm.provider", Kind::ProviderId, "llm", "Which provider serves a model that names none. Empty means the endpoint above.", Some("THETIS_PROVIDER"), Choices::Providers),
    // --- agent --------------------------------------------------------------
    fe("agent.max_iterations", Kind::Int, "agent", "Tool-call rounds allowed in a single turn.", "THETIS_MAX_ITERATIONS"),
    fc("agent.default_mode", Kind::ModeId, "agent", "The mode a new conversation starts in.", Some("THETIS_DEFAULT_MODE"), Choices::Modes),
    fe("agent.name", Kind::Text, "agent", "What the agent calls itself: in the system prompt, the top-left brand, and the Discord bot's username. Empty means Thetis.", "THETIS_AGENT_NAME"),
    fe("agent.avatar", Kind::LongText, "agent", "A URL or data: URI shown beside the brand. Empty draws the built-in mark.", "THETIS_AGENT_AVATAR"),
    fe("agent.system_prompt", Kind::LongText, "agent", "The system prompt. {agent_name} is substituted. Ignored when a prompt file is set; empty means the built-in prompt.", "THETIS_SYSTEM_PROMPT"),
    f("agent.system_prompt_file", Kind::Path, "agent", "A file holding the system prompt, read at boot. Wins over the inline prompt."),
    // --- subagents ----------------------------------------------------------
    fe("subagents.enabled", Kind::Bool, "subagents", "Whether the agent may spawn sub-agents at all.", "THETIS_SUBAGENTS"),
    f("subagents.max_children", Kind::Int, "subagents", "Live children one conversation may have at once."),
    f("subagents.max_wait_secs", Kind::Int, "subagents", "Longest a parent waits on a child before giving up on it."),
    f("subagents.max_result_bytes", Kind::Int, "subagents", "Cap on what a child hands back to its parent."),
    fc("subagents.default_mode", Kind::ModeId, "subagents", "The mode a child runs in unless its profile says otherwise. Empty means agent.default_mode.", None, Choices::Modes),
    // --- budgets ------------------------------------------------------------
    fe("budgets.wasm_slice_secs", Kind::Int, "budgets", "CPU a guest may burn in one slice before it is interrupted.", "THETIS_WASM_SLICE_SECS"),
    fe("budgets.tool_secs", Kind::Int, "budgets", "Wall-clock a single tool invocation gets.", "THETIS_TOOL_BUDGET_SECS"),
    fe("budgets.probe_secs", Kind::Int, "budgets", "Wall-clock for a probe call: describe, health, serve-asset.", "THETIS_PROBE_BUDGET_SECS"),
    // --- limits -------------------------------------------------------------
    fe("limits.agent_memory_mb", Kind::Int, "limits", "Linear memory ceiling for the agent component.", "THETIS_AGENT_MEM_MB"),
    fe("limits.tool_memory_mb", Kind::Int, "limits", "Linear memory ceiling for a tool component.", "THETIS_TOOL_MEM_MB"),
    fe("limits.gateway_memory_mb", Kind::Int, "limits", "Linear memory ceiling for the gateway component.", "THETIS_GATEWAY_MEM_MB"),
    fe("limits.session_spend_limit_usd", Kind::Float, "limits", "A conversation stops taking turns past this cumulative cost. 0 means no limit.", "THETIS_SESSION_SPEND_LIMIT_USD"),
    fe("limits.max_tool_output_bytes", Kind::Int, "limits", "A tool result larger than this is spilled to a file and summarised.", "THETIS_MAX_TOOL_OUTPUT"),
    fe("limits.max_attachment_bytes", Kind::Int, "limits", "Largest single attachment a message may carry.", "THETIS_MAX_ATTACHMENT_BYTES"),
    fe("limits.max_attachments", Kind::Int, "limits", "Most attachments on one message.", "THETIS_MAX_ATTACHMENTS"),
    // --- context ------------------------------------------------------------
    fe("context.enabled", Kind::Bool, "context", "Compact the conversation when it nears the window, instead of failing at the cliff.", "THETIS_COMPACT"),
    fe("context.window_tokens", Kind::Int, "context", "The context window compaction plans against. Deliberately below any real window.", "THETIS_CONTEXT_WINDOW"),
    f("context.compact_threshold", Kind::Float, "context", "Fraction of the window at which compaction starts (0.05 to 1.0)."),
    f("context.compact_target", Kind::Float, "context", "Fraction of the window to compact down to (0.01 to 0.95)."),
    fc("context.summary_model", Kind::ModelId, "context", "The model that writes the summary. Empty means the conversation's own.", Some("THETIS_SUMMARY_MODEL"), Choices::Models),
    f("context.keep_head", Kind::Int, "context", "Messages at the start that are never compacted away."),
    f("context.keep_tail", Kind::Int, "context", "Most recent messages kept verbatim."),
    // --- cache --------------------------------------------------------------
    fe("cache.enabled", Kind::Bool, "cache", "Prompt caching: reuse the unchanged prefix of the request across turns.", "THETIS_CACHE"),
    fe("cache.ttl", Kind::Text, "cache", "How long a cached prefix is asked to live, e.g. 5m or 1h.", "THETIS_CACHE_TTL"),
    f("cache.anchor_stride", Kind::Int, "cache", "Place an explicit cache breakpoint every this many messages."),
    f("cache.explicit_vendors", Kind::List, "cache", "Vendors that cache nothing unless told to. Others cache automatically, and explicit marks there bill writes."),
    // --- skills -------------------------------------------------------------
    fe("skills.retrieval_enabled", Kind::Bool, "skills", "Rank skills against the opening message and pin the best few to the conversation.", "THETIS_SKILL_RETRIEVAL"),
    f("skills.retrieve_limit", Kind::Int, "skills", "How many retrieved skills a conversation gets (1 to 50)."),
    fe("skills.embedding_model", Kind::Text, "skills", "The embedding model retrieval ranks with.", "THETIS_EMBEDDING_MODEL"),
    fc("skills.embedding_provider", Kind::ProviderId, "skills", "Which provider serves embeddings. Empty means the default.", Some("THETIS_EMBEDDING_PROVIDER"), Choices::Providers),
    f("skills.embedding_dimensions", Kind::Int, "skills", "Vector width the model returns (64 to 4096). Changing it re-embeds the corpus."),
    f("skills.max_query_chars", Kind::Int, "skills", "Most of the opening message that is embedded."),
    f("skills.max_universal", Kind::Int, "skills", "Most always-present skill briefs (up to 20)."),
    // --- tool_groups --------------------------------------------------------
    fe("tool_groups.grouping_enabled", Kind::Bool, "tool_groups", "Offer the model tool groups and let it attach the ones a turn needs, instead of every tool at once.", "THETIS_TOOL_GROUPING"),
    f("tool_groups.accounting_enabled", Kind::Bool, "tool_groups", "Record which groups each turn used, for the Tools inspector."),
    f("tool_groups.always_on", Kind::List, "tool_groups", "Group ids that are attached to every turn regardless."),
    f("tool_groups.route_threshold", Kind::Float, "tool_groups", "Score a group needs to be attached automatically (0.0 to 1.0)."),
    // --- build --------------------------------------------------------------
    fe("build.command", Kind::Text, "build", "The cargo binary used to build guests.", "THETIS_BUILD_COMMAND"),
    fe("build.target", Kind::Text, "build", "The wasm target triple.", "THETIS_BUILD_TARGET"),
    fe("build.profile", Kind::Text, "build", "The cargo profile guests are built with.", "THETIS_BUILD_PROFILE"),
    fe("build.target_dir", Kind::Path, "build", "Where guest builds go. Shared by the fleet.", "THETIS_TARGET_DIR"),
    f("build.locked", Kind::Bool, "build", "Pass --locked, so a guest build cannot silently change its dependency tree."),
    f("build.extra_args", Kind::List, "build", "Extra arguments appended to every guest build."),
    fe("build.timeout_secs", Kind::Int, "build", "A build slower than this is killed. Generous: cold builds fetch dependencies.", "THETIS_BUILD_TIMEOUT_SECS"),
    f("build.allowed_crates", Kind::List, "build", "Crates a guest may depend on. Empty means anything."),
    // --- wasi ---------------------------------------------------------------
    fe("wasi.network", Kind::Bool, "wasi", "Whether guests may open sockets.", "THETIS_WASI_NETWORK"),
    fe("wasi.dns", Kind::Bool, "wasi", "Whether guests may resolve names.", "THETIS_WASI_DNS"),
    fe("wasi.env", Kind::Bool, "wasi", "Whether guests see the host environment. Off on purpose: the API keys live there.", "THETIS_WASI_ENV"),
    fe("wasi.stdio", Kind::Bool, "wasi", "Whether guests inherit stdio.", "THETIS_WASI_STDIO"),
    fe("wasi.dirs", Kind::List, "wasi", "Directories preopened for guests. The first is the shared workspace.", "THETIS_WORKSPACE_DIR"),
    // --- watchdog -----------------------------------------------------------
    f("watchdog.failure_window_secs", Kind::Int, "watchdog", "Window over which guest failures are counted."),
    f("watchdog.failure_threshold", Kind::Int, "watchdog", "Failures within the window before a component is rolled back."),
    f("watchdog.probe_interval_secs", Kind::Int, "watchdog", "How often each component's health is probed."),
    f("watchdog.watch_suppression_secs", Kind::Int, "watchdog", "How long the file watcher ignores a component after it was swapped."),
    f("watchdog.debounce_ms", Kind::Int, "watchdog", "How long the file watcher waits for edits to settle before building."),
    // --- devkit -------------------------------------------------------------
    fe("devkit.enabled", Kind::Bool, "devkit", "Whether the agent gets its self-modification tools.", "THETIS_DEVKIT"),
    f("devkit.protected_files", Kind::List, "devkit", "Files the dev kit refuses to edit."),
    f("devkit.protected_dirs", Kind::List, "devkit", "Directories the dev kit refuses to touch."),
    // --- sandbox ------------------------------------------------------------
    fe("sandbox.enabled", Kind::Bool, "sandbox", "Whether the command sandbox is offered to tools.", "THETIS_SANDBOX"),
    // --- filesystem ---------------------------------------------------------
    fe("filesystem.enabled", Kind::Bool, "filesystem", "Whether the agent gets host filesystem tools.", "THETIS_FILESYSTEM"),
    f("filesystem.roots", Kind::List, "filesystem", "Directories the file tools may reach. Keep \".\" first: relative paths resolve against it. Empty means the project root."),
    f("filesystem.max_read_bytes", Kind::Int, "filesystem", "Largest file a single read returns."),
    f("filesystem.protected", Kind::List, "filesystem", "Paths under a root the file tools refuse to write."),
    f("filesystem.allow_delete", Kind::Bool, "filesystem", "Whether the file tools may delete."),
    // --- terminal -----------------------------------------------------------
    fe("terminal.enabled", Kind::Bool, "terminal", "Whether the agent may open shells on this machine.", "THETIS_TERMINAL"),
    f("terminal.shell", Kind::Text, "terminal", "The shell to run. Empty picks one for the platform."),
    f("terminal.shell_args", Kind::List, "terminal", "Arguments for the shell. Empty picks a quiet default."),
    f("terminal.max_sessions", Kind::Int, "terminal", "Shells one conversation may hold open."),
    f("terminal.default_timeout_ms", Kind::Int, "terminal", "How long a command waits for output before returning what it has."),
    f("terminal.max_output_bytes", Kind::Int, "terminal", "Most output returned from one command."),
    f("terminal.idle_timeout_secs", Kind::Int, "terminal", "An idle shell is closed after this."),
    fe("terminal.ssh_enabled", Kind::Bool, "terminal", "Whether remote shells may be opened to hosts in the ssh registry.", "THETIS_TERMINAL_SSH"),
    f("terminal.ssh_program", Kind::Text, "terminal", "The ssh binary."),
    f("terminal.ssh_connect_timeout_ms", Kind::Int, "terminal", "How long a remote shell has to connect and start."),
    f("terminal.send_settle_ms", Kind::Int, "terminal", "How long to wait for a prompt to settle after sending keystrokes."),
    // --- control ------------------------------------------------------------
    fe("control.allow_restart", Kind::Bool, "control", "Whether the agent and this panel may restart the orchestrator.", "THETIS_ALLOW_RESTART"),
    f("control.min_uptime_secs", Kind::Int, "control", "Restarts are refused before this uptime, so a failing restart cannot become a loop."),
    f("control.restart_delay_ms", Kind::Int, "control", "Pause between accepting a restart and doing it, so the reason can be read."),
    // --- discord ------------------------------------------------------------
    fe("discord.enabled", Kind::Bool, "discord", "Run the Discord connector.", "DISCORD_ENABLED"),
    fe("discord.bot_token", Kind::Secret, "discord", "The bot token. The environment variable overrides it when set.", "DISCORD_BOT_TOKEN"),
    fc("discord.mode", Kind::ModeId, "discord", "The mode Discord conversations run in. Must be read-only.", Some("DISCORD_MODE"), Choices::Modes),
    fe("discord.allowed_users", Kind::List, "discord", "Discord user ids that may talk to the bot.", "DISCORD_ALLOWED_USERS"),
    fe("discord.admin_users", Kind::List, "discord", "Discord user ids that may issue pairing codes. Empty means everyone allowed may.", "DISCORD_ADMIN_USERS"),
    fe("discord.allow_all_users", Kind::Bool, "discord", "Answer anyone, not just the allowlist.", "DISCORD_ALLOW_ALL_USERS"),
    fe("discord.require_mention", Kind::Bool, "discord", "In channels, answer only when mentioned.", "DISCORD_REQUIRE_MENTION"),
    fe("discord.free_response_channels", Kind::List, "discord", "Channel ids where no mention is needed.", "DISCORD_FREE_RESPONSE_CHANNELS"),
    fe("discord.ignore_no_mention", Kind::Bool, "discord", "Stay silent, rather than explain, when not mentioned.", "DISCORD_IGNORE_NO_MENTION"),
    fe("discord.group_sessions_per_user", Kind::Bool, "discord", "One conversation per user per channel, instead of one per channel.", "DISCORD_GROUP_SESSIONS_PER_USER"),
    f("discord.stream_edit_interval_ms", Kind::Int, "discord", "How often a streaming reply's message is edited."),
    f("discord.pairing_code_ttl_secs", Kind::Int, "discord", "How long a pairing code stays valid."),
    fe("discord.allow_fork", Kind::Bool, "discord", "Let a paired account fork a Discord conversation into one that runs under its own web policy, which can write.", "DISCORD_ALLOW_FORK"),
    // --- browser ------------------------------------------------------------
    fe("browser.enabled", Kind::Bool, "browser", "Run the headless browser sidecar for the web-browser tools.", "THETIS_BROWSER_ENABLED"),
    fe("browser.port", Kind::Int, "browser", "Loopback port the sidecar listens on.", "THETIS_BROWSER_PORT"),
    f("browser.service_dir", Kind::Path, "browser", "The sidecar's Node project."),
    fe("browser.node", Kind::Path, "browser", "The node binary. Empty finds it on PATH.", "THETIS_BROWSER_NODE"),
    fe("browser.npm", Kind::Path, "browser", "The npm binary. Empty finds it on PATH.", "THETIS_BROWSER_NPM"),
    fe("browser.playwright_version", Kind::Text, "browser", "Pinned, so the browser build the install step checks for does not float.", "THETIS_BROWSER_PLAYWRIGHT_VERSION"),
    fe("browser.auto_install", Kind::Bool, "browser", "Run npm install for the sidecar at boot when it is missing.", "THETIS_BROWSER_AUTO_INSTALL"),
    f("browser.install_timeout_secs", Kind::Int, "browser", "Bound on a cold install."),
    f("browser.startup_timeout_secs", Kind::Int, "browser", "How long the sidecar has to come up."),
    f("browser.default_timeout_ms", Kind::Int, "browser", "Default timeout for a browser action."),
    f("browser.idle_timeout_secs", Kind::Int, "browser", "An idle page is closed after this."),
    f("browser.snapshot_chars", Kind::Int, "browser", "Most characters of a page snapshot returned to the model."),
    f("browser.artifact_dir", Kind::Path, "browser", "Where screenshots and downloads land. Inside the workspace so guests can reach them."),
];

pub fn field(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

/// A column of a list section.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub key: &'static str,
    pub kind: Kind,
    pub help: &'static str,
    pub required: bool,
    pub choices: Choices,
}

const fn c(key: &'static str, kind: Kind, help: &'static str) -> Column {
    Column { key, kind, help, required: false, choices: Choices::None }
}
const fn req(key: &'static str, kind: Kind, help: &'static str) -> Column {
    Column { key, kind, help, required: true, choices: Choices::None }
}
const fn cc(key: &'static str, kind: Kind, help: &'static str, choices: Choices) -> Column {
    Column { key, kind, help, required: false, choices }
}

/// A section that is a list of tables, keyed by `id`.
#[derive(Debug, Clone, Copy)]
pub struct TableSection {
    /// Dotted path of the array: `models`, `subagents.profiles`.
    pub id: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub columns: &'static [Column],
    /// Whether a new list belongs in the local overlay rather than the
    /// committed file: accounts and their hashes never enter version control.
    pub local: bool,
}

/// The capability ids a role may deny. Mirrors `policy::Cap`.
pub const CAPABILITIES: &[&str] = &[
    "filesystem_read",
    "filesystem_write",
    "filesystem_delete",
    "terminal",
    "ssh",
    "devkit",
    "control",
    "config_write",
    "branch_write",
    "delegation",
    "skills_write",
    "transcripts",
    "component_tools",
    "sandbox",
    "workspace",
    "workspace_write",
];

/// The fields of a `PolicyLayer`, shared by roles and user overrides.
pub const POLICY_COLUMNS: &[Column] = &[
    c("admin", Kind::Bool, "Administrator: every capability, every conversation, this panel."),
    c("read_only", Kind::Bool, "Nothing that writes: files, shells, branches, configuration, delegation."),
    cc("deny_capabilities", Kind::List, "Capabilities withheld outright. Hard host boundaries.", Choices::Static(CAPABILITIES)),
    cc("models", Kind::List, "The closed set of models offered. Empty means any.", Choices::Models),
    cc("default_model", Kind::ModelId, "The model new conversations start with.", Choices::Models),
    cc("modes", Kind::List, "The closed set of modes offered. Empty means any.", Choices::Modes),
    cc("default_mode", Kind::ModeId, "The mode new conversations start in.", Choices::Modes),
    c("deny_tools", Kind::List, "Tool names hidden and refused. Advisory for the agent's built-ins."),
    c("deny_groups", Kind::List, "Tool groups hidden and refused. Advisory; never `core`."),
    c("spend_limit_usd", Kind::Float, "Cumulative spend across the account's conversations. 0 means unlimited."),
    c("max_children", Kind::Int, "Sub-agents one conversation may run at once."),
    c("see_all_sessions", Kind::Bool, "May switch the sidebar to everyone's conversations."),
];

const MODEL_COLUMNS: &[Column] = &[
    req("id", Kind::Text, "The name used everywhere: the picker, the session, THETIS_MODEL."),
    c("label", Kind::Text, "Shown in the picker. Empty falls back to the id."),
    cc("provider", Kind::ProviderId, "Which provider serves it. Empty means the default.", Choices::Providers),
    c("wire_model", Kind::Text, "What to send as `model` when it differs from the id."),
];

const MODE_COLUMNS: &[Column] = &[
    req("id", Kind::Text, "The mode's id."),
    c("label", Kind::Text, "Shown in the picker. Empty falls back to the id."),
    c("description", Kind::Text, "One line under the label."),
    c("read_only", Kind::Bool, "Withhold every tool that changes something, and refuse them at dispatch."),
    c("prompt", Kind::LongText, "Appended to the system prompt. {agent_name} is substituted."),
];

const PROVIDER_COLUMNS: &[Column] = &[
    req("id", Kind::Text, "Routing prefix: `local/qwen3` goes to the provider called `local`. Avoid OpenRouter vendor names."),
    c("label", Kind::Text, "Shown where the provider is named. Empty falls back to the id."),
    c("base_url", Kind::Url, "One endpoint. Give this or base_urls, not both."),
    c("base_urls", Kind::List, "Replicas of the same model, used round-robin."),
    c("api_key", Kind::Secret, "Literal key, empty for none, or env:NAME to read the environment."),
    c("headers", Kind::Map, "Extra request headers sent to this provider."),
];

const ROLE_COLUMNS: &[Column] = &[
    req("id", Kind::Text, "The role's id, named by users."),
    c("description", Kind::Text, "What the role is for."),
    c("admin", Kind::Bool, "Administrator: every capability, every conversation, this panel."),
    c("read_only", Kind::Bool, "Nothing that writes: files, shells, branches, configuration, delegation."),
    cc("deny_capabilities", Kind::List, "Capabilities withheld outright. Hard host boundaries.", Choices::Static(CAPABILITIES)),
    cc("models", Kind::List, "The closed set of models offered. Empty means any.", Choices::Models),
    cc("default_model", Kind::ModelId, "The model new conversations start with.", Choices::Models),
    cc("modes", Kind::List, "The closed set of modes offered. Empty means any.", Choices::Modes),
    cc("default_mode", Kind::ModeId, "The mode new conversations start in.", Choices::Modes),
    c("deny_tools", Kind::List, "Tool names hidden and refused. Advisory for the agent's built-ins."),
    c("deny_groups", Kind::List, "Tool groups hidden and refused. Advisory; never `core`."),
    c("spend_limit_usd", Kind::Float, "Cumulative spend across the account's conversations. 0 means unlimited."),
    c("max_children", Kind::Int, "Sub-agents one conversation may run at once."),
    c("see_all_sessions", Kind::Bool, "May switch the sidebar to everyone's conversations."),
];

const USER_COLUMNS: &[Column] = &[
    req("id", Kind::Text, "Lower-case letters, digits, dots, dashes, underscores; how the person signs in."),
    c("name", Kind::Text, "Display name. Empty falls back to the id."),
    cc("role", Kind::RoleId, "The role whose policy this account inherits.", Choices::Roles),
    c("password", Kind::Secret, "A new password. Hashed here and stored as password_hash; never read back."),
    c("password_env", Kind::Text, "An environment variable holding the hash instead of the file."),
    c("discord_id", Kind::Text, "The Discord account bound to this one, for /fork."),
    c("overrides", Kind::Map, "Per-user narrowing of the role's policy: the same fields a role has."),
];

const PROFILE_COLUMNS: &[Column] = &[
    req("id", Kind::Text, "The profile's id, named when delegating."),
    c("label", Kind::Text, "Shown where the profile is offered."),
    c("description", Kind::Text, "What this kind of sub-agent is for."),
    cc("model", Kind::ModelId, "The model the child runs on. Must be a configured model.", Choices::Models),
    cc("mode", Kind::ModeId, "The mode the child runs in. Must be a configured mode.", Choices::Modes),
    c("prompt", Kind::LongText, "Appended to the child's system prompt."),
];

pub const TABLES: &[TableSection] = &[
    TableSection { id: "models", label: "Models", help: "The catalogue the picker offers. Empty means a built-in list. A model added from the Models tab lives in the database instead, per user, and needs no restart.", columns: MODEL_COLUMNS, local: false },
    TableSection { id: "providers", label: "Providers", help: "Extra OpenAI-compatible endpoints beside the default one. An entry whose id is `openrouter` replaces it.", columns: PROVIDER_COLUMNS, local: false },
    TableSection { id: "modes", label: "Modes", help: "What a conversation may be set to. Empty means the built-in agent and plan modes.", columns: MODE_COLUMNS, local: false },
    TableSection { id: "roles", label: "Roles", help: "Named policies users inherit. A role is an administrator only if it says so.", columns: ROLE_COLUMNS, local: true },
    TableSection { id: "users", label: "Users", help: "Accounts. Each needs exactly one password source. Kept in the local overlay, out of version control.", columns: USER_COLUMNS, local: true },
    TableSection { id: "subagents.profiles", label: "Sub-agent profiles", help: "Named ways to delegate: a model, a mode and a prompt.", columns: PROFILE_COLUMNS, local: false },
];

pub fn table(id: &str) -> Option<&'static TableSection> {
    TABLES.iter().find(|t| t.id == id)
}

/// Sections that hold scalars, in the order the panel shows them.
pub const SECTIONS: &[(&str, &str, &str)] = &[
    ("server", "Server", "The listener, the gateway that serves the UI, and the recovery console."),
    ("auth", "Authentication", "Accounts and sign-in."),
    ("llm", "Language model", "The default endpoint and model."),
    ("agent", "Agent", "Identity, prompt and turn limits."),
    ("subagents", "Sub-agents", "Delegation limits."),
    ("limits", "Limits", "Memory, spend and payload ceilings."),
    ("budgets", "Budgets", "Time a guest may take."),
    ("context", "Context", "Compaction."),
    ("cache", "Prompt cache", "Prefix caching."),
    ("skills", "Skills", "Retrieval and embeddings."),
    ("tool_groups", "Tool groups", "Grouping the tool surface."),
    ("filesystem", "Filesystem", "Host file access."),
    ("terminal", "Terminal", "Shells, local and remote."),
    ("wasi", "WASI", "What guests may reach."),
    ("sandbox", "Sandbox", "The command sandbox."),
    ("devkit", "Dev kit", "Self-modification."),
    ("browser", "Browser", "The headless browser sidecar."),
    ("discord", "Discord", "The Discord connector."),
    ("build", "Build", "How guests are compiled."),
    ("watchdog", "Watchdog", "Failure detection and rollback."),
    ("control", "Control", "Restarting."),
    ("paths", "Paths", "Where things live. Moving data or artifacts orphans what is already there."),
    ("tools", "Tool settings", "Free-form per-tool blocks, handed to each tool as its configuration."),
];

pub fn section_label(id: &str) -> &'static str {
    SECTIONS
        .iter()
        .find(|(s, _, _)| *s == id)
        .map(|(_, l, _)| *l)
        .unwrap_or("Other")
}
