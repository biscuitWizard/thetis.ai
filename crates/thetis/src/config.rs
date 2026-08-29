//! Configuration.
//!
//! Three layers, each overriding the one before it:
//!
//!   1. the defaults in this file, so Thetis runs with no configuration at all
//!   2. `thetis.toml` (or `$THETIS_CONFIG`), the normal place to change things
//!   3. environment variables, for per-run overrides and secrets
//!
//! A key this build does not recognise is reported and ignored rather than
//! fatal. The agent edits this file and the code that reads it together, and
//! the binary that understands a new section is built by a process that only
//! runs while the service is up - so a config one step ahead of its binary
//! must not be able to hold the service down.
//!
//! The OpenRouter key may be set in the file or in the environment, with the
//! environment winning. It is held in a `Secret`, whose `Debug` prints nothing,
//! so it cannot reach a log through an incidental `{:?}`.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::aspect::Aspect;

// ---------------------------------------------------------------------------
// Runtime configuration
// ---------------------------------------------------------------------------

/// A value that must never be printed.
///
/// `Debug` is implemented by hand: deriving it anywhere up the tree — on
/// `Config`, or on something holding a `Config` — would otherwise be enough to
/// spill the key into a log line.
#[derive(Clone, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "Secret(empty)" } else { "Secret(***)" })
    }
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// How the model is named everywhere inside Thetis: in the picker, in a
    /// session's stored model, in `THETIS_MODEL`.
    pub id: String,
    pub label: String,
    /// Which `[[providers]]` entry serves it. Empty means `llm.provider`.
    pub provider: String,
    /// What goes in the request's `model` field, when that differs from `id`.
    /// A local llama.cpp server usually wants a bare name where the picker
    /// wants something namespaced, so the two are allowed to differ.
    pub wire_model: String,
}

impl ModelSpec {
    /// The name to put on the wire for this model.
    pub fn wire(&self) -> &str {
        if self.wire_model.is_empty() {
            &self.id
        } else {
            &self.wire_model
        }
    }
}

/// The id of the provider synthesized from `[llm]`, and the default when
/// nothing names one.
pub const DEFAULT_PROVIDER_ID: &str = "openrouter";

/// One OpenAI-compatible endpoint: OpenRouter, a local llama.cpp server, an
/// OpenAI-shaped gateway of any kind.
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub id: String,
    pub label: String,
    /// One or more interchangeable base URLs, without trailing slashes.
    /// `/chat/completions` and `/embeddings` are appended to one of them.
    ///
    /// More than one entry means replicas of the same model: several
    /// llama-server processes on different ports, or different machines.
    /// Requests are handed out round-robin, so a provider scales by gaining
    /// entries here rather than by being duplicated under a new id — the
    /// model ids in the picker do not change when you add capacity.
    ///
    /// Never empty; `base_url()` is always safe.
    pub base_urls: Vec<String>,
    /// Absent means send no `Authorization` header at all, which is what an
    /// unauthenticated local server wants — an empty bearer token is rejected
    /// by some and silently mishandled by others.
    pub api_key: Option<Secret>,
    /// Extra headers on every request to this provider.
    pub headers: Vec<(String, String)>,
}

impl ProviderSpec {
    /// The first endpoint. Use this for identity and for display; use
    /// `url()` to actually address the provider, so replicas are used.
    pub fn base_url(&self) -> &str {
        self.base_urls
            .first()
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// How many interchangeable endpoints serve this provider.
    pub fn replicas(&self) -> usize {
        self.base_urls.len()
    }

    /// A request URL against a specific replica, chosen by `index` modulo the
    /// number of endpoints. The caller owns the counter, so the rotation is
    /// shared across a whole process rather than restarting per request.
    pub fn url_for(&self, path: &str, index: usize) -> String {
        let base = if self.base_urls.is_empty() {
            ""
        } else {
            self.base_urls[index % self.base_urls.len()].as_str()
        };
        format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    /// A request URL against the first endpoint, for callers with no counter.
    pub fn url(&self, path: &str) -> String {
        self.url_for(path, 0)
    }

    /// OpenRouter attributes requests with these; nobody else wants them.
    pub fn is_openrouter(&self) -> bool {
        self.base_url().contains("openrouter.ai")
    }
}

/// A model request resolved to the endpoint that will serve it.
#[derive(Debug, Clone)]
pub struct ResolvedModel<'a> {
    /// The name to send as `model`.
    pub wire_model: String,
    pub provider: &'a ProviderSpec,
}

#[derive(Debug, Clone)]
pub struct ModeSpec {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Whether tools that change things are withheld in this mode. Carried to
    /// the agent so a new mode needs no agent code.
    pub read_only: bool,
    /// Appended to the system prompt while this mode is active.
    ///
    /// Withholding tools tells the model what it cannot do, but never what it
    /// should do instead - it just meets the gap and works around it. This is
    /// where a mode says what it is for.
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub artifacts: PathBuf,
    pub skills: PathBuf,
    pub templates: PathBuf,
    pub wit: PathBuf,
    /// The agent's source tree.
    pub agent: PathBuf,
    /// Directory holding gateway crates, and the prefix their names carry.
    pub gateways: PathBuf,
    pub gateway_prefix: String,
    pub tools: PathBuf,
    pub tool_prefix: String,
    /// Where per-conversation sandbox checkouts live. Gateway-only: workers
    /// run *inside* one of these.
    pub worktrees: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BuildSettings {
    pub command: String,
    pub target: String,
    pub profile: String,
    /// Cargo target directory for guest builds, resolved against the config's
    /// own root — so each worktree compiles into its own.
    ///
    /// Deliberately not shared between checkouts. Cargo only rewrites an output
    /// when the copy it is building is dirty, so one shared directory let two
    /// branches serve each other's artifacts, and the workaround for that
    /// (dirtying a source file before every build) fed the file watcher a fake
    /// edit and span cargo forever. Dependencies are still compiled once per
    /// worktree rather than once per fleet; cross-branch reuse comes from the
    /// content-addressed build cache in `paths.artifacts`, which is shared.
    pub target_dir: PathBuf,
    /// Pass `--locked` when a lockfile exists, keeping resolution reproducible.
    pub locked: bool,
    pub extra_args: Vec<String>,
    /// How long a single cargo invocation may run before it is killed.
    ///
    /// Builds hold a process-wide lock, so one that never returns - a stalled
    /// crates.io fetch, a dependency's `build.rs` waiting on input - would wedge
    /// every future build permanently. This is the backstop for that.
    pub timeout: Duration,
    /// Crates a guest may depend on. Empty means no restriction.
    pub allowed_crates: Vec<String>,
}

/// What the WASI sandbox actually hands to a guest.
///
/// Guests run on WASI preview 2, which is capability-based: the interfaces are
/// linked either way, but they do nothing at all until a capability is granted.
/// Before this existed the context was built empty, so a tool could link
/// `reqwest`, compile it, and then find no sockets underneath at runtime.
#[derive(Debug, Clone)]
pub struct WasiSettings {
    /// Outbound sockets and `wasi:http`. Any tool calling a web API needs it.
    pub network: bool,
    /// Name resolution. Without it a guest can only reach literal addresses.
    pub dns: bool,
    /// The host's environment variables.
    pub env: bool,
    /// The host's stdin, stdout and stderr.
    pub stdio: bool,
    /// Directories handed to guests as preopens, readable and writable.
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchdogSettings {
    pub failure_window: Duration,
    pub failure_threshold: usize,
    pub probe_interval: Duration,
    /// How long the file watcher ignores an aspect the orchestrator just wrote to.
    pub watch_suppression: Duration,
    pub debounce: Duration,
}

#[derive(Debug, Clone)]
pub struct DevkitSettings {
    pub enabled: bool,
    /// Files that decide what runs during a build; a host-side build executes
    /// them, so guests may not edit them.
    pub protected_files: Vec<String>,
    pub protected_dirs: Vec<String>,
}

/// How a provider's prompt cache is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStrategy {
    /// The provider caches long prefixes on its own; marking would only cost
    /// writes. Keeping the prefix stable is all that helps.
    Automatic,
    /// Nothing is cached unless a breakpoint says so.
    Breakpoints,
}

/// When and how a long conversation is summarized down.
///
/// The trigger reads the provider's own `usage.prompt_tokens` from the last
/// turn, which is the whole input it saw - system prompt, tool schemas and all
/// history - so it measures the thing that actually costs money, not an
/// estimate of one part of it.
#[derive(Debug, Clone)]
pub struct ContextSettings {
    pub enabled: bool,
    /// Assumed usable context window, in tokens.
    pub window: u32,
    /// Fraction of the window that triggers compaction.
    pub compact_threshold: f64,
    /// Fraction of the window to compact back down to. Compaction stops at the
    /// first round boundary under this, so it sheds what is needed and no more.
    pub compact_target: f64,
    /// A cheaper model for the summary itself. Empty uses the session's model.
    pub summary_model: String,
    /// Messages at the start of the conversation kept verbatim.
    pub keep_head: u32,
    /// Messages at the end kept verbatim - the agent's live working memory.
    pub keep_tail: u32,
}

#[derive(Debug, Clone)]
pub struct CacheSettings {
    pub enabled: bool,
    /// Cache lifetime, e.g. "5m" or "1h". A longer one costs more to write.
    pub ttl: String,
    /// How far apart the stable anchors sit in the message list. Smaller means
    /// more resilience to turns that add many blocks, at the cost of more
    /// frequent writes.
    pub anchor_stride: usize,
    /// Vendors that need explicit breakpoints. Anything else is left automatic.
    pub explicit_vendors: Vec<String>,
}

impl CacheSettings {
    pub fn strategy_for(&self, vendor: &str) -> CacheStrategy {
        if self
            .explicit_vendors
            .iter()
            .any(|v| v.eq_ignore_ascii_case(vendor))
        {
            CacheStrategy::Breakpoints
        } else {
            CacheStrategy::Automatic
        }
    }
}

/// How skills are retrieved and injected.
#[derive(Debug, Clone)]
pub struct SkillSettings {
    /// Master switch. Off means no embedding calls and no L1 block: only the
    /// L0 briefs of universal skills reach the prompt.
    pub retrieval_enabled: bool,
    /// How many L1 cards to inject for a conversation.
    pub retrieve_limit: usize,
    /// Embedding model. Must honour the `dimensions` parameter.
    pub embedding_model: String,
    /// Which provider serves embeddings. Empty routes by the model id, exactly
    /// as a chat model does.
    pub embedding_provider: String,
    /// Vector width requested from the provider.
    pub embedding_dimensions: u32,
    /// Opening-message prefix that gets embedded as the retrieval query.
    pub max_query_chars: usize,
    /// Ceiling on skills marked `universal`, past which discovery warns.
    pub max_universal: usize,
}

impl Default for SkillSettings {
    fn default() -> Self {
        Self {
            retrieval_enabled: true,
            retrieve_limit: 10,
            embedding_model: "openai/text-embedding-3-small".into(),
            embedding_provider: String::new(),
            embedding_dimensions: 1536,
            max_query_chars: 2000,
            max_universal: 20,
        }
    }
}

/// How the tool surface is scoped to a conversation.
///
/// Tools are grouped, and a group is admitted to a session's surface only when
/// something indicates it is wanted: it is always-on, a retrieved skill points
/// at it, or its tags match the opening message. The research this implements is
/// consistent that a large flat tool list costs both tokens and *accuracy* —
/// but also that a naive filter can lose, so the default is off and the
/// measurement it needs is always on.
#[derive(Debug, Clone)]
pub struct ToolGroupSettings {
    /// Master switch. Off means every tool is offered, exactly as before, and
    /// only the per-turn accounting runs.
    pub grouping_enabled: bool,
    /// Log the token cost of the tool block and which tools were actually
    /// called, each turn. Independent of `grouping_enabled` on purpose: the
    /// baseline is what makes the change judgeable.
    pub accounting_enabled: bool,
    /// Groups admitted for every session regardless of routing, beyond the ones
    /// that declare themselves always-on in code.
    pub always_on: Vec<String>,
    /// Minimum lexical score for the opening message to admit a group on tag
    /// evidence alone. Deliberately generous: a group wrongly admitted costs
    /// tokens, while one wrongly withheld costs a capability.
    pub route_threshold: f64,
}

impl Default for ToolGroupSettings {
    fn default() -> Self {
        Self {
            grouping_enabled: false,
            accounting_enabled: true,
            always_on: Vec::new(),
            route_threshold: 0.15,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilesystemSettings {
    pub enabled: bool,
    /// Every path the agent touches must resolve inside one of these.
    pub roots: Vec<PathBuf>,
    pub max_read_bytes: usize,
    /// Names that may not be written or deleted anywhere under a root. This
    /// protects the system's own state from an accidental `rm -rf`, and is not
    /// a security boundary — a terminal session can reach them regardless.
    pub protected: Vec<String>,
    pub allow_delete: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalSettings {
    pub enabled: bool,
    pub shell: String,
    pub shell_args: Vec<String>,
    pub max_sessions: usize,
    pub default_timeout: Duration,
    pub max_output_bytes: usize,
    pub idle_timeout: Duration,
    /// Whether a session may be opened against a registered ssh host.
    ///
    /// Separate from `enabled` because it is a wider boundary: a local shell
    /// reaches this machine, a remote one reaches whatever the keys on this
    /// machine can reach. The host registry is not a substitute for this
    /// switch — turning it off disables remote sessions with the hosts still
    /// defined.
    pub ssh_enabled: bool,
    pub ssh_program: String,
    /// How long a new remote session has to answer its first command before it
    /// is declared unusable and torn down.
    pub ssh_connect_timeout: Duration,
    /// How long `send` waits for a reply before returning what arrived.
    pub send_settle: Duration,
}

#[derive(Debug, Clone)]
pub struct ControlSettings {
    pub allow_restart: bool,
    /// A restart is refused before this much uptime, so a misbehaving agent
    /// cannot put the process into a restart loop.
    pub min_uptime: Duration,
    /// How long to wait after the call so the turn can finish and the user can
    /// read why.
    pub restart_delay: Duration,
}

/// The Discord bot connector.
///
/// Off unless a token is present. The `mode` is the safety property: every
/// session the connector creates is stamped with it, and the agent withholds
/// mutating tools for any mode marked `read_only`. The connector refuses to
/// start if that mode is missing or is not read-only, because
/// `read_only()` in the agent treats an unknown mode as full access -- so a
/// typo here would otherwise hand a public chat surface the dev kit.
/// The headless browser sidecar that backs the `web-browser-*` tools.
///
/// Tool components are wasm and cannot spawn processes, so none of them can
/// drive Playwright directly. The kernel runs one Node sidecar on loopback and
/// the tools speak JSON to it; this is where that process is configured.
///
/// Headless is not a setting. There is no display on a host running Thetis, and
/// a headed browser would simply hang, so the sidecar hardcodes it.
#[derive(Debug, Clone)]
pub struct BrowserSettings {
    pub enabled: bool,
    /// Loopback port. Never bound on a public interface.
    pub port: u16,
    /// The sidecar's directory, holding `package.json` and `server.js`.
    pub service_dir: PathBuf,
    /// `node` and `npm` binaries. Empty means look on PATH.
    pub node: String,
    pub npm: String,
    /// The playwright version to install and check for.
    pub playwright_version: String,
    /// Whether boot may run `npm install`. Off means verify and warn only.
    pub auto_install: bool,
    pub install_timeout: Duration,
    pub startup_timeout: Duration,
    /// Default per-operation timeout inside the browser.
    pub default_timeout_ms: u64,
    /// How long an unused browser context is kept.
    pub idle_timeout_secs: u64,
    /// Characters of accessibility snapshot returned before trimming.
    pub snapshot_chars: usize,
    /// Where screenshots and PDFs are written.
    pub artifact_dir: PathBuf,
}

impl BrowserSettings {
    /// The loopback base URL tools are told to call.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn node_bin(&self) -> &str {
        if self.node.trim().is_empty() { "node" } else { self.node.trim() }
    }

    pub fn npm_bin(&self) -> &str {
        if self.npm.trim().is_empty() { "npm" } else { self.npm.trim() }
    }
}

#[derive(Debug, Clone)]
pub struct DiscordSettings {
    pub enabled: bool,
    pub bot_token: Option<Secret>,
    /// Session mode stamped on every Discord conversation. Must be read-only.
    pub mode: String,
    /// Discord user ids always allowed to talk to the bot.
    pub allowed_users: Vec<String>,
    /// Users who may issue `/pair` to authorise someone else. Empty means
    /// everyone in `allowed_users` may.
    pub admin_users: Vec<String>,
    /// Allow anyone to talk to the bot. Dangerous on a public server.
    pub allow_all_users: bool,
    /// Require an @mention in guild channels. DMs never require one.
    pub require_mention: bool,
    /// Channel ids where a mention is not required.
    pub free_response_channels: Vec<String>,
    /// Stay silent when a message mentions other people but not the bot.
    pub ignore_no_mention: bool,
    /// Give each user their own session inside a shared channel.
    pub group_sessions_per_user: bool,
    /// Minimum gap between edits to a streaming reply. Discord rate-limits
    /// edits, so this is well above per-token.
    pub stream_edit_interval: Duration,
    /// Longest a pairing code stays valid.
    pub pairing_code_ttl: Duration,
}

impl DiscordSettings {
    /// Whether this user may talk to the bot at all.
    pub fn authorized(&self, user_id: &str, paired: &[String]) -> bool {
        self.allow_all_users
            || self.allowed_users.iter().any(|u| u == user_id)
            || paired.iter().any(|u| u == user_id)
    }

    /// Whether this user may authorise others. Falls back to the static
    /// allowlist so a fresh install has an admin without extra configuration;
    /// a paired user never becomes one, or pairing would be self-propagating.
    pub fn is_admin(&self, user_id: &str) -> bool {
        if self.admin_users.is_empty() {
            self.allowed_users.iter().any(|u| u == user_id)
        } else {
            self.admin_users.iter().any(|u| u == user_id)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Project root: the directory holding the config file and the source trees.
    pub root: PathBuf,
    /// The file this was loaded from, and the one edits are written back to.
    /// It need not exist: absent means everything came from defaults.
    pub config_path: PathBuf,
    /// Per-tool settings, keyed by tool name, from `[tools.<name>]`. Each tool
    /// is handed its own block and never sees another's.
    pub tools: std::collections::BTreeMap<String, toml::Value>,
    pub paths: Paths,
    pub bind_addr: SocketAddr,
    /// Gateway aspect that serves the browser UI.
    pub primary_gateway: String,
    pub admin_enabled: bool,

    // --- llm ---------------------------------------------------------------
    pub openrouter_api_key: Option<Secret>,
    pub openrouter_base: String,
    pub model: String,
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub models: Vec<ModelSpec>,
    /// Every configured endpoint. Always non-empty: the first entry is the one
    /// built from `[llm]`, so a config that names no provider behaves exactly
    /// as it did before providers existed.
    pub providers: Vec<ProviderSpec>,
    /// Which provider serves a model that does not name one.
    pub default_provider: String,

    // --- agent loop --------------------------------------------------------
    /// What the agent calls itself: in its system prompt, in the web UI's
    /// brand, and as the Discord bot's username.
    ///
    /// Deliberately separate from the harness, which is always called Thetis.
    /// The two were the same word for a long time and it made every sentence
    /// about either one ambiguous.
    pub agent_name: String,
    /// An image to show as the agent's avatar in the web UI, beside the brand.
    ///
    /// A URL or a `data:` URI. Empty means the built-in mark, which is drawn as
    /// an SVG and tints itself with the accent colour.
    pub agent_avatar: String,
    pub system_prompt: String,
    pub max_iterations: u32,
    pub modes: Vec<ModeSpec>,
    pub default_mode: String,

    // --- budgets -----------------------------------------------------------
    pub wasm_slice: Duration,
    pub tool_budget: Duration,
    pub probe_budget: Duration,

    // --- ceilings ----------------------------------------------------------
    pub agent_memory_bytes: usize,
    pub tool_memory_bytes: usize,
    pub gateway_memory_bytes: usize,
    pub session_spend_limit_usd: f64,
    pub max_tool_output_bytes: usize,
    pub max_attachment_bytes: usize,
    pub max_attachments: usize,

    pub context: ContextSettings,
    pub cache: CacheSettings,
    pub skills: SkillSettings,
    pub tool_groups: ToolGroupSettings,
    pub build: BuildSettings,
    pub wasi: WasiSettings,
    pub watchdog: WatchdogSettings,
    pub devkit: DevkitSettings,
    pub filesystem: FilesystemSettings,
    pub terminal: TerminalSettings,
    pub control: ControlSettings,
    pub discord: DiscordSettings,
    pub browser: BrowserSettings,
    pub sandbox_available: bool,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.paths.data.join("thetis.redb")
    }

    /// A provider by id, or `None` when nothing is configured under that name.
    pub fn provider(&self, id: &str) -> Option<&ProviderSpec> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// The provider used when a model names none. Falls back to the first
    /// configured one, which always exists.
    pub fn fallback_provider(&self) -> &ProviderSpec {
        self.provider(&self.default_provider)
            .or_else(|| self.providers.first())
            .expect("providers is never empty")
    }

    /// Works out which endpoint serves a model id, and under what name.
    ///
    /// Three ways a model reaches a provider, in order:
    ///
    /// 1. a `provider = "..."` prefix on the id (`local/qwen3` where `local`
    ///    is a configured provider), which is how a model can be used without
    ///    being listed in `[[models]]` at all;
    /// 2. a matching `[[models]]` entry naming a provider;
    /// 3. the default provider, which is OpenRouter unless changed.
    ///
    /// A prefix that is not a configured provider is left alone — `anthropic/`
    /// in an OpenRouter id must not be mistaken for a provider name.
    pub fn resolve_model(&self, model: &str) -> ResolvedModel<'_> {
        let model = model.trim();

        if let Some(spec) = self.models.iter().find(|m| m.id == model) {
            if !spec.provider.is_empty() {
                if let Some(provider) = self.provider(&spec.provider) {
                    return ResolvedModel {
                        wire_model: spec.wire().to_string(),
                        provider,
                    };
                }
                tracing::warn!(
                    model = %model,
                    provider = %spec.provider,
                    "model names an unconfigured provider; using the default"
                );
            }
            return ResolvedModel {
                wire_model: spec.wire().to_string(),
                provider: self.fallback_provider(),
            };
        }

        // Not listed: an id may still address a provider by prefix.
        if let Some((prefix, rest)) = model.split_once('/') {
            if !rest.is_empty() {
                if let Some(provider) = self.provider(prefix) {
                    return ResolvedModel {
                        wire_model: rest.to_string(),
                        provider,
                    };
                }
            }
        }

        ResolvedModel {
            wire_model: model.to_string(),
            provider: self.fallback_provider(),
        }
    }

    /// An aspect's source directory relative to the checkout root — the path
    /// git knows it by. `None` when it sits outside the checkout.
    pub fn aspect_source_rel(&self, aspect: &Aspect) -> Option<String> {
        self.aspect_source_dir(aspect)
            .strip_prefix(&self.root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    }

    /// The cross-process build lock. Lives next to the database because both
    /// name process-wide ground truth, not per-checkout state.
    pub fn build_lock_path(&self) -> PathBuf {
        self.paths.data.join("build.lock")
    }

    /// The cross-process lock for orchestrator builds.
    ///
    /// Separate from the guest build lock: a kernel build is minutes of four
    /// cores and gigabytes of target directory, while a guest build is
    /// seconds, so making them queue behind each other would stall every
    /// conversation's edit-compile loop. Also in the shared data directory,
    /// because the point is to serialize across *workers*, each of which is
    /// its own process with its own in-process mutex.
    pub fn kernel_build_lock_path(&self) -> PathBuf {
        self.paths.data.join("kernel-build.lock")
    }

    /// Source directory for an aspect.
    pub fn aspect_source_dir(&self, aspect: &Aspect) -> PathBuf {
        match aspect {
            Aspect::Agent => self.paths.agent.clone(),
            Aspect::Gateway(name) => self
                .paths
                .gateways
                .join(format!("{}{name}", self.paths.gateway_prefix)),
            Aspect::Tool(name) => self
                .paths
                .tools
                .join(format!("{}{name}", self.paths.tool_prefix)),
        }
    }

    /// Cargo package name of the crate backing an aspect, taken from its directory
    /// so the convention lives in configuration rather than in code.
    pub fn aspect_crate_name(&self, aspect: &Aspect) -> String {
        self.aspect_source_dir(aspect)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| aspect.key())
    }

    /// Filename cargo emits for an aspect's component.
    pub fn aspect_wasm_filename(&self, aspect: &Aspect) -> String {
        format!("{}.wasm", self.aspect_crate_name(aspect).replace('-', "_"))
    }

    pub fn aspect_artifact_dir(&self, aspect: &Aspect, revision: u64) -> PathBuf {
        self.paths
            .artifacts
            .join(aspect.artifact_subdir())
            .join(format!("r{revision:04}"))
    }

    /// One tool's settings as JSON, or `{}` when it has none configured.
    ///
    /// Scoped deliberately: a tool is handed its own block and cannot read
    /// another's, nor anything else in the configuration.
    ///
    /// A hyphenated name also inherits from its group: `notion-search` reads
    /// `[tools.notion]` and then `[tools.notion-search]`, with the more
    /// specific block winning key by key. That is what lets a family of tools
    /// sharing one API credential name it once instead of once per tool, while
    /// a tool with no hyphen behaves exactly as before.
    ///
    /// Each scope may also be filled from the environment, which wins over the
    /// file: `NOTION_API_KEY` or `THETIS_TOOL_NOTION_TOKEN` both land as
    /// `token` in the `notion` scope. A secret held in the environment never
    /// has to be written into a config file to be usable.
    pub fn tool_config_json(&self, tool: &str) -> String {
        let mut merged: Option<toml::Value> = None;
        for scope in tool_config_scopes(tool) {
            if let Some(block) = self.tools.get(&scope) {
                match merged.as_mut() {
                    Some(base) => merge_toml(base, block.clone()),
                    None => merged = Some(block.clone()),
                }
            }
            // The environment wins over the file at the same scope, so a
            // credential can stay out of the repository entirely.
            if let Some(from_env) = tool_env_overlay(&scope) {
                match merged.as_mut() {
                    Some(base) => merge_toml(base, from_env),
                    None => merged = Some(from_env),
                }
            }
        }
        if let Some(table) = merged.as_mut() {
            self.inline_file_secrets(table);
        }
        // The browser tools need to know where the sidecar is and hold the
        // token for it. Both are runtime facts — the port is settings-derived
        // and the token is generated fresh each boot — so they are injected
        // here rather than written into a file a user would have to keep in
        // step. A value the user did set explicitly still wins.
        if tool.starts_with("web-browser") {
            let table = merged.get_or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let toml::Value::Table(t) = table {
                t.entry("endpoint".to_string())
                    .or_insert_with(|| toml::Value::String(self.browser.base_url()));
                t.entry("token".to_string())
                    .or_insert_with(|| toml::Value::String(crate::browser::token(self).to_string()));
                t.entry("enabled".to_string())
                    .or_insert_with(|| toml::Value::Boolean(self.browser.enabled));
            }
        }
        merged
            .and_then(|v| serde_json::to_string(&v).ok())
            .unwrap_or_else(|| "{}".to_string())
    }

    /// The directories a `*_path` tool secret may be read from, most specific
    /// first.
    ///
    /// The project root is the obvious one. The shared overlay's directory is
    /// the necessary one: a conversation runs from a git worktree, and the
    /// overlay naming the credential lives outside it (see the
    /// `THETIS_LOCAL_CONFIG` handling in [`Self::load`]). Without this, a key
    /// referenced by the shared overlay would have to be copied into every
    /// branch — exactly what pointing workers at a shared overlay was meant to
    /// avoid. Both roots are trusted already: one holds the source being run,
    /// the other holds the secrets.
    fn secret_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        if let Some(dir) = env_string("THETIS_LOCAL_CONFIG")
            .map(|p| resolve(&self.root, &p))
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            if !roots.contains(&dir) {
                roots.push(dir);
            }
        }
        roots
    }

    /// Reads any `*_path` secret referenced by a tool block and inlines its
    /// contents beside it as `*_contents`.
    ///
    /// A tool has no filesystem import, by design: `world tool` imports only
    /// `sys` and `sandbox`. But a credential that is genuinely a *file* — an RSA
    /// private key is the motivating case — should not have to be pasted into
    /// TOML as a mangled single line with `\n` escapes. So the host, which does
    /// hold filesystem authority, reads it on the tool's behalf.
    ///
    /// Only keys ending in `_path` are considered, the value must resolve inside
    /// one of the roots from [`Self::secret_roots`], and a failure is reported in
    /// `*_contents_error` rather than thrown — a tool can then explain the
    /// problem in its own terms instead of silently behaving as if nothing were
    /// configured.
    fn inline_file_secrets(&self, block: &mut toml::Value) {
        let Some(table) = block.as_table_mut() else {
            return;
        };

        let paths: Vec<(String, String)> = table
            .iter()
            .filter_map(|(key, value)| {
                let key = key.strip_suffix("_path")?;
                let raw = value.as_str()?.trim();
                (!raw.is_empty()).then(|| (key.to_string(), raw.to_string()))
            })
            .collect();

        let roots = self.secret_roots();

        for (stem, raw) in paths {
            // Try each root in turn and keep the first hit, so a relative path
            // works whether the key lives beside the branch's config or beside
            // the shared overlay. Falls back to the project root, which is what
            // the error message should name when nothing was found.
            let resolved = roots
                .iter()
                .map(|root| resolve(root, &raw))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| resolve(&self.root, &raw));

            // Confinement: the same rule the hostfs interface applies. A tool
            // config must not become a way to read /etc/shadow.
            //
            // `canonicalize` fails on a path that does not exist, so a missing
            // file and an escaping one are distinguished before the check —
            // otherwise a simple typo is reported as an attempt to break out
            // of the project, which sends the reader somewhere very wrong.
            let outcome = match resolved.canonicalize() {
                Err(e) => Err(format!("{}: {e}", resolved.display())),
                Ok(canonical) => {
                    let inside = roots
                        .iter()
                        .filter_map(|root| root.canonicalize().ok())
                        .any(|root| canonical.starts_with(&root));
                    if inside {
                        std::fs::read_to_string(&canonical).map_err(|e| format!("{e}"))
                    } else {
                        Err(format!(
                            "{} does not resolve inside the project root",
                            resolved.display()
                        ))
                    }
                }
            };

            match outcome {
                Ok(contents) => {
                    table.insert(
                        format!("{stem}_contents"),
                        toml::Value::String(contents),
                    );
                }
                Err(error) => {
                    table.insert(
                        format!("{stem}_contents_error"),
                        toml::Value::String(error),
                    );
                }
            }
        }
    }

    pub fn mode(&self, id: &str) -> Option<&ModeSpec> {
        self.modes.iter().find(|m| m.id == id)
    }

    /// Directories the file watcher follows.
    pub fn watched_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.paths.agent.clone(),
            self.paths.gateways.clone(),
            self.paths.tools.clone(),
            self.paths.wit.clone(),
        ]
    }
}

// ---------------------------------------------------------------------------
// File shape
// ---------------------------------------------------------------------------

mod spec {
    use serde::Deserialize;

    // No `Debug` on this or on `Llm`: both hold the raw API key, and a derived
    // Debug anywhere above them would be enough to print it.
    #[derive(Default, Deserialize)]
    #[serde(default)]
    pub struct File {
        pub server: Server,
        pub paths: Paths,
        pub skills: Skills,
        pub tool_groups: ToolGroups,
        pub llm: Llm,
        pub agent: Agent,
        pub providers: Vec<Provider>,
        pub models: Vec<Model>,
        pub modes: Vec<Mode>,
        pub budgets: Budgets,
        pub limits: Limits,
        pub context: Context,
        pub cache: Cache,
        pub build: Build,
        pub watchdog: Watchdog,
        pub devkit: Devkit,
        pub sandbox: Sandbox,
        pub filesystem: Filesystem,
        pub terminal: Terminal,
        pub control: Control,
        pub discord: Discord,
        pub browser: Browser,
        pub wasi: Wasi,
        /// Free-form per-tool settings. Shapes are up to each tool, so this is
        /// carried as-is rather than being given a schema here.
        pub tools: std::collections::BTreeMap<String, toml::Value>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Server {
        pub bind: String,
        pub primary_gateway: String,
        pub admin_enabled: bool,
    }
    impl Default for Server {
        fn default() -> Self {
            Self {
                bind: "127.0.0.1:7777".into(),
                primary_gateway: "web".into(),
                admin_enabled: true,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Paths {
        pub data: String,
        pub artifacts: String,
        pub skills: String,
        pub templates: String,
        pub wit: String,
        pub agent: String,
        pub gateways: String,
        pub gateway_prefix: String,
        pub tools: String,
        pub tool_prefix: String,
        pub worktrees: String,
    }
    impl Default for Paths {
        fn default() -> Self {
            Self {
                data: "data".into(),
                artifacts: "artifacts".into(),
                skills: "skills".into(),
                templates: "templates".into(),
                wit: "wit".into(),
                agent: "agents/agent-core".into(),
                gateways: "gateways".into(),
                gateway_prefix: "gateway-".into(),
                tools: "tools".into(),
                tool_prefix: "".into(),
                worktrees: "worktrees".into(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Skills {
        pub retrieval_enabled: bool,
        pub retrieve_limit: usize,
        pub embedding_model: String,
        pub embedding_provider: String,
        pub embedding_dimensions: u32,
        pub max_query_chars: usize,
        pub max_universal: usize,
    }
    impl Default for Skills {
        fn default() -> Self {
            let d = super::SkillSettings::default();
            Self {
                retrieval_enabled: d.retrieval_enabled,
                retrieve_limit: d.retrieve_limit,
                embedding_model: d.embedding_model,
                embedding_provider: d.embedding_provider,
                embedding_dimensions: d.embedding_dimensions,
                max_query_chars: d.max_query_chars,
                max_universal: d.max_universal,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct ToolGroups {
        pub grouping_enabled: bool,
        pub accounting_enabled: bool,
        pub always_on: Vec<String>,
        pub route_threshold: f64,
    }
    impl Default for ToolGroups {
        fn default() -> Self {
            let d = super::ToolGroupSettings::default();
            Self {
                grouping_enabled: d.grouping_enabled,
                accounting_enabled: d.accounting_enabled,
                always_on: d.always_on,
                route_threshold: d.route_threshold,
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(default)]
    pub struct Llm {
        pub base_url: String,
        pub model: String,
        pub api_key: String,
        pub request_timeout_secs: u64,
        pub max_retries: u32,
        /// Which `[[providers]]` entry serves a model that names none. Empty
        /// means the one built from this section.
        pub provider: String,
    }
    impl Default for Llm {
        fn default() -> Self {
            Self {
                base_url: "https://openrouter.ai/api/v1".into(),
                model: "anthropic/claude-sonnet-4.5".into(),
                api_key: String::new(),
                request_timeout_secs: 180,
                max_retries: 3,
                provider: String::new(),
            }
        }
    }

    /// One OpenAI-compatible endpoint. No `Debug`: it holds an API key.
    #[derive(Deserialize)]
    pub struct Provider {
        pub id: String,
        #[serde(default)]
        pub label: String,
        /// A single endpoint. Give this or `base_urls`, not both.
        #[serde(default)]
        pub base_url: String,
        /// Interchangeable replicas of the same model, used round-robin. This
        /// is how a provider scales: adding a port here adds capacity without
        /// changing any model id.
        #[serde(default)]
        pub base_urls: Vec<String>,
        /// Literal key, or empty for an unauthenticated endpoint. A value of
        /// the form `env:NAME` is read from that environment variable instead,
        /// so a real key need not sit in the file.
        #[serde(default)]
        pub api_key: String,
        /// Extra request headers, e.g. `{ "X-Org" = "acme" }`.
        #[serde(default)]
        pub headers: std::collections::BTreeMap<String, String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Agent {
        pub max_iterations: u32,
        pub default_mode: String,
        /// What the agent calls itself. Empty falls back to the default.
        pub name: String,
        /// Image URL or `data:` URI for the agent's avatar. Empty draws the
        /// built-in mark instead.
        pub avatar: String,
        /// Inline prompt. Ignored when `system_prompt_file` is set.
        pub system_prompt: String,
        /// Path to a prompt file, relative to the project root.
        pub system_prompt_file: String,
    }
    impl Default for Agent {
        fn default() -> Self {
            Self {
                max_iterations: 32,
                default_mode: "agent".into(),
                name: String::new(),
                avatar: String::new(),
                system_prompt: String::new(),
                system_prompt_file: String::new(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    
    pub struct Model {
        pub id: String,
        #[serde(default)]
        pub label: String,
        /// Which provider serves it. Empty means the default one.
        #[serde(default)]
        pub provider: String,
        /// What to send as `model` when it differs from `id`.
        #[serde(default)]
        pub wire_model: String,
    }

    #[derive(Debug, Deserialize)]
    
    pub struct Mode {
        pub id: String,
        #[serde(default)]
        pub label: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub read_only: bool,
        #[serde(default)]
        pub prompt: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Budgets {
        pub wasm_slice_secs: u64,
        pub tool_secs: u64,
        pub probe_secs: u64,
    }
    impl Default for Budgets {
        fn default() -> Self {
            Self {
                wasm_slice_secs: 10,
                tool_secs: 30,
                probe_secs: 5,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Limits {
        pub agent_memory_mb: usize,
        pub tool_memory_mb: usize,
        pub gateway_memory_mb: usize,
        pub session_spend_limit_usd: f64,
        pub max_tool_output_bytes: usize,
        pub max_attachment_bytes: usize,
        pub max_attachments: usize,
    }
    impl Default for Limits {
        fn default() -> Self {
            Self {
                agent_memory_mb: 512,
                tool_memory_mb: 128,
                gateway_memory_mb: 128,
                session_spend_limit_usd: 0.0,
                max_tool_output_bytes: 32_768,
                max_attachment_bytes: 8_388_608,
                max_attachments: 8,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Context {
        pub enabled: bool,
        pub window_tokens: u32,
        pub compact_threshold: f64,
        pub compact_target: f64,
        pub summary_model: String,
        pub keep_head: u32,
        pub keep_tail: u32,
    }
    impl Default for Context {
        fn default() -> Self {
            Self {
                enabled: true,
                // Deliberately below any real window: the point is to compact
                // well before the provider starts refusing, not at the cliff.
                window_tokens: 200_000,
                compact_threshold: 0.6,
                compact_target: 0.25,
                // Empty means "whatever the session is using".
                summary_model: String::new(),
                keep_head: 4,
                keep_tail: 30,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Cache {
        pub enabled: bool,
        pub ttl: String,
        pub anchor_stride: usize,
        pub explicit_vendors: Vec<String>,
    }
    impl Default for Cache {
        fn default() -> Self {
            Self {
                enabled: true,
                ttl: "5m".into(),
                anchor_stride: 8,
                // Anthropic caches nothing without being told to. OpenAI and
                // Google do it themselves, and explicit marks there bill writes
                // for prefixes that move every turn.
                explicit_vendors: vec!["anthropic".into()],
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Build {
        pub command: String,
        pub target: String,
        pub profile: String,
        pub target_dir: String,
        pub locked: bool,
        pub extra_args: Vec<String>,
        pub timeout_secs: u64,
        pub allowed_crates: Vec<String>,
    }
    impl Default for Build {
        fn default() -> Self {
            Self {
                command: "cargo".into(),
                target: "wasm32-wasip2".into(),
                profile: "release".into(),
                target_dir: "target-wasm".into(),
                locked: true,
                extra_args: Vec::new(),
                // Generous: a cold build that fetches a large dependency tree
                // is slow but legitimate. This only catches genuine hangs.
                timeout_secs: 900,
                allowed_crates: Vec::new(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Wasi {
        pub network: bool,
        pub dns: bool,
        pub env: bool,
        pub stdio: bool,
        pub dirs: Vec<String>,
    }
    impl Default for Wasi {
        fn default() -> Self {
            Self {
                // Permissive: a tool that cannot reach the network cannot be a
                // web tool, and the component boundary is still the sandbox.
                network: true,
                dns: true,
                // Deliberately not permissive. The host environment is where
                // the API keys live, and no guest has a reason to read them.
                env: false,
                stdio: false,
                dirs: vec!["workspace".into()],
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Watchdog {
        pub failure_window_secs: u64,
        pub failure_threshold: usize,
        pub probe_interval_secs: u64,
        pub watch_suppression_secs: u64,
        pub debounce_ms: u64,
    }
    impl Default for Watchdog {
        fn default() -> Self {
            Self {
                failure_window_secs: 120,
                failure_threshold: 3,
                probe_interval_secs: 30,
                watch_suppression_secs: 5,
                debounce_ms: 500,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Devkit {
        pub enabled: bool,
        pub protected_files: Vec<String>,
        pub protected_dirs: Vec<String>,
    }
    impl Default for Devkit {
        fn default() -> Self {
            Self {
                enabled: true,
                // Nothing is protected. A component that cannot edit its own
                // manifest cannot add a dependency; add entries here only for a
                // deployment that wants a component held still.
                protected_files: Vec::new(),
                protected_dirs: Vec::new(),
            }
        }
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(default)]
    pub struct Sandbox {
        pub enabled: bool,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Filesystem {
        pub enabled: bool,
        pub roots: Vec<String>,
        pub max_read_bytes: usize,
        pub protected: Vec<String>,
        pub allow_delete: bool,
    }
    impl Default for Filesystem {
        fn default() -> Self {
            Self {
                enabled: true,
                // Empty means "the project root", resolved at load time.
                roots: Vec::new(),
                max_read_bytes: 1_048_576,
                protected: ["data", "artifacts", ".git"].map(String::from).to_vec(),
                allow_delete: true,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Terminal {
        pub enabled: bool,
        pub shell: String,
        pub shell_args: Vec<String>,
        pub max_sessions: usize,
        pub default_timeout_ms: u64,
        pub max_output_bytes: usize,
        pub idle_timeout_secs: u64,
        pub ssh_enabled: bool,
        pub ssh_program: String,
        pub ssh_connect_timeout_ms: u64,
        pub send_settle_ms: u64,
    }
    impl Default for Terminal {
        fn default() -> Self {
            Self {
                enabled: true,
                // Empty means "whatever suits this platform".
                shell: String::new(),
                shell_args: Vec::new(),
                max_sessions: 4,
                default_timeout_ms: 30_000,
                max_output_bytes: 65_536,
                idle_timeout_secs: 1_800,
                // On by default, and the reason is that it grants nothing on
                // its own: a remote session needs a host in the registry, and
                // adding one is a deliberate act. Nobody's reach widens until
                // they define somewhere to reach.
                ssh_enabled: true,
                ssh_program: "ssh".into(),
                // A connect, an auth handshake and a shell start, over a link
                // that may be slow. Generous, because the failure it guards is
                // a session that never works at all.
                ssh_connect_timeout_ms: 25_000,
                send_settle_ms: 400,
            }
        }
    }

    /// The headless browser sidecar. See [`super::BrowserSettings`].
    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Browser {
        pub enabled: bool,
        pub port: u16,
        pub service_dir: String,
        pub node: String,
        pub npm: String,
        pub playwright_version: String,
        pub auto_install: bool,
        pub install_timeout_secs: u64,
        pub startup_timeout_secs: u64,
        pub default_timeout_ms: u64,
        pub idle_timeout_secs: u64,
        pub snapshot_chars: usize,
        pub artifact_dir: String,
    }
    impl Default for Browser {
        fn default() -> Self {
            Self {
                enabled: true,
                // Loopback only, and well clear of the usual dev-server ports.
                port: 39412,
                service_dir: "services/playwright-sidecar".into(),
                // Empty means "find it on PATH".
                node: String::new(),
                npm: String::new(),
                // Pinned deliberately: this version's browser build is the one
                // the install step checks for, so a floating version would mean
                // an unexpected browser download on some later boot.
                playwright_version: "1.61.0".into(),
                auto_install: true,
                // A cold `npm install` fetches playwright; a warm one is
                // instant. This bounds only the cold case.
                install_timeout_secs: 300,
                startup_timeout_secs: 45,
                default_timeout_ms: 15_000,
                idle_timeout_secs: 900,
                // A tool result is capped at 32 KB and a snapshot is only part
                // of one, so leave room for the rest of the response.
                snapshot_chars: 12_000,
                // Inside `workspace` so the wasm guests' preopen can reach it.
                artifact_dir: "workspace/browser".into(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    pub struct Control {
        pub allow_restart: bool,
        pub min_uptime_secs: u64,
        pub restart_delay_ms: u64,
    }
    impl Default for Control {
        fn default() -> Self {
            Self {
                allow_restart: true,
                min_uptime_secs: 20,
                restart_delay_ms: 1_500,
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(default)]
    pub struct Discord {
        pub enabled: bool,
        pub bot_token: String,
        pub mode: String,
        pub allowed_users: Vec<String>,
        pub admin_users: Vec<String>,
        pub allow_all_users: bool,
        pub require_mention: bool,
        pub free_response_channels: Vec<String>,
        pub ignore_no_mention: bool,
        pub group_sessions_per_user: bool,
        pub stream_edit_interval_ms: u64,
        pub pairing_code_ttl_secs: u64,
    }
    impl Default for Discord {
        fn default() -> Self {
            Self {
                enabled: true,
                bot_token: String::new(),
                mode: "chat".to_string(),
                allowed_users: Vec::new(),
                admin_users: Vec::new(),
                allow_all_users: false,
                require_mention: true,
                free_response_channels: Vec::new(),
                ignore_no_mention: true,
                group_sessions_per_user: true,
                stream_edit_interval_ms: 1_200,
                pairing_code_ttl_secs: 900,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Where [`Config::assemble`] reads its overrides from.
///
/// Injected rather than read straight from the process, because the variables
/// that override configuration are the same ones the gateway pins into every
/// worker it spawns — `THETIS_DATA_DIR`, `THETIS_ARTIFACTS_DIR`,
/// `THETIS_TARGET_DIR` and the rest. A test that builds a config from a TOML
/// string and asserts on the result would silently read the *host's* state
/// instead of the file under test: green on a developer's shell, red inside a
/// conversation, which is precisely where Thetis runs its own suite.
///
/// Tests that mean "this file, and nothing else" pass [`Env::None`]. Nothing
/// else changes: the overrides still apply everywhere they did before.
#[derive(Clone, Copy, Debug)]
enum Env {
    /// The process environment, as the running orchestrator sees it.
    Process,
    /// No overrides at all — the configuration file speaks for itself.
    /// Only the tests ask for this; production always reads the process.
    #[cfg_attr(not(test), allow(dead_code))]
    None,
}

impl Env {
    fn string(self, key: &str) -> Option<String> {
        match self {
            Self::Process => env_string(key),
            Self::None => Option::None,
        }
    }

    fn parse<T: std::str::FromStr>(self, key: &str, current: T) -> T {
        match self {
            Self::Process => env_parse(key, current),
            Self::None => current,
        }
    }

    fn list(self, key: &str) -> Option<Vec<String>> {
        match self {
            Self::Process => env_list(key),
            Self::None => Option::None,
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, current: T) -> T {
    env_string(key).and_then(|v| v.parse().ok()).unwrap_or(current)
}

/// A comma-separated environment list, as the Discord allowlists are given.
///
/// `None` when the variable is absent, so the caller keeps its file value; an
/// explicit empty string yields an empty list, which is how a list configured
/// in the file is deliberately cleared for one run.
fn env_list(key: &str) -> Option<Vec<String>> {
    std::env::var(key).ok().map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    })
}

/// Walks up from `start` looking for the marker that identifies a project root.
fn discover_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("thetis.toml").is_file() || dir.join("wit").is_dir())
        .map(Path::to_path_buf)
}

/// Joins a configured path onto the root unless it is already absolute.
/// `thetis.toml` -> `thetis.local.toml`, keeping any other stem intact.
fn local_overlay_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "thetis".to_string());
    config_path.with_file_name(format!("{stem}.local.toml"))
}

fn read_toml(path: &Path) -> Result<toml::Value> {
    if !path.is_file() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Deep-merges `overlay` into `base`, with the overlay winning.
///
/// Recursive on tables so an overlay can set one key of one tool without
/// restating the whole section. Arrays are replaced rather than concatenated:
/// appending would make it impossible to shorten a list from the overlay.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Config blocks a tool reads, least specific first.
///
/// `notion-search` yields ["notion", "notion-search"]; `web-search` yields
/// ["web", "web-search"]. A group block that nobody wrote simply does not
/// exist, so the extra scope costs nothing.
fn tool_config_scopes(tool: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut prefix = String::new();
    for part in tool.split('-') {
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(part);
        scopes.push(prefix.clone());
    }
    if scopes.is_empty() {
        scopes.push(tool.to_string());
    }
    scopes
}

/// Settings for one tool scope taken from the environment.
///
/// Two spellings are accepted for each scope, because both are things an
/// operator will reasonably reach for:
///
/// * `THETIS_TOOL_<SCOPE>_<KEY>` — the explicit, general form. Any key can be
///   set this way, not just a credential.
/// * `<SCOPE>_API_KEY` and `<SCOPE>_TOKEN` — the conventional name a service's
///   own documentation uses, mapped onto `token`. This is what makes dropping
///   `NOTION_API_KEY` into an env file work with no further wiring.
///
/// Hyphens in a scope become underscores, since a hyphen cannot appear in an
/// environment variable name. Values arrive as strings; a tool that wants a
/// number parses it, which the clients already do for `timeout_secs`.
fn tool_env_overlay(scope: &str) -> Option<toml::Value> {
    let upper = scope.to_ascii_uppercase().replace('-', "_");
    let mut table = toml::map::Map::new();

    // The conventional per-service spellings, mapped onto `token`.
    for suffix in ["API_KEY", "TOKEN", "API_TOKEN", "ACCESS_TOKEN"] {
        if let Some(value) = env_string(&format!("{upper}_{suffix}")) {
            table.insert("token".to_string(), toml::Value::String(value));
            break;
        }
    }

    // The explicit form, which can set any key and takes precedence.
    let prefix = format!("THETIS_TOOL_{upper}_");
    for (name, value) in std::env::vars() {
        let Some(key) = name.strip_prefix(&prefix) else {
            continue;
        };
        if key.is_empty() || value.trim().is_empty() {
            continue;
        }
        table.insert(key.to_ascii_lowercase(), toml::Value::String(value));
    }

    (!table.is_empty()).then(|| toml::Value::Table(table))
}

/// Deserializes the config, collecting what the schema did not recognise rather
/// than refusing outright.
///
/// An unknown key used to be fatal, which is defensible in an ordinary service
/// and wrong here. The agent edits thetis.toml and config.rs together, and the
/// binary that understands a new section is built by a process that only runs
/// while the service is up - so a config one step ahead of its binary would
/// refuse to start, and the thing that would fix it could not run. That is a
/// deadlock only an outside hand can break.
///
/// So an unrecognised key is now a warning: the section is ignored, its
/// defaults apply, and the next rebuild picks it up. A typo still gets said out
/// loud; it just no longer takes the system down with it.
fn parse_file(value: toml::Value) -> Result<(spec::File, Vec<String>)> {
    use serde::de::IntoDeserializer;

    let mut unknown = Vec::new();
    let file = serde_ignored::deserialize(value.into_deserializer(), |path| {
        unknown.push(path.to_string())
    })
    .map_err(|e: toml::de::Error| anyhow::anyhow!(e))?;
    Ok((file, unknown))
}

fn resolve(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir().context("resolving the current directory")?;
        let root = match env_string("THETIS_ROOT") {
            Some(r) => PathBuf::from(r),
            None => discover_root(&cwd).unwrap_or(cwd),
        };

        let config_path = match env_string("THETIS_CONFIG") {
            Some(p) => resolve(&root, &p),
            None => root.join("thetis.toml"),
        };

        let mut merged = read_toml(&config_path)?;

        // `thetis.toml` is committed; the local overlay beside it is not.
        // Secrets that belong to a tool - an API key for a service it calls -
        // have nowhere else to go, since a tool's settings are read from the
        // config file rather than the environment.
        let local_path = local_overlay_path(&config_path);
        if local_path.is_file() {
            merge_toml(&mut merged, read_toml(&local_path)?);
            tracing::debug!(overlay = %local_path.display(), "applied local config overlay");
        }

        // Workers run from worktrees, where the (gitignored) local overlay
        // does not exist; the gateway points them at the shared one so
        // secrets never need to enter a branch.
        if let Some(shared_overlay) = env_string("THETIS_LOCAL_CONFIG") {
            let shared_overlay = resolve(&root, &shared_overlay);
            if shared_overlay != local_path && shared_overlay.is_file() {
                merge_toml(&mut merged, read_toml(&shared_overlay)?);
                tracing::debug!(overlay = %shared_overlay.display(), "applied shared config overlay");
            }
        }

        let (file, unknown) = parse_file(merged)
            .with_context(|| format!("parsing {}", config_path.display()))?;
        if !unknown.is_empty() {
            tracing::warn!(
                keys = %unknown.join(", "),
                "configuration has keys this build does not understand; ignoring them"
            );
        }

        Self::assemble(root, config_path, file, Env::Process)
    }

    /// The overlay that sits beside a config file: `thetis.toml` becomes
    /// `thetis.local.toml`.
    pub fn local_overlay(&self) -> PathBuf {
        local_overlay_path(&self.config_path)
    }

    /// Checks that a candidate config file would load, without applying it.
    ///
    /// Thetis refuses to start on a bad configuration, so writing one is the
    /// one mistake that cannot be undone from inside the running system. Every
    /// edit goes through here first.
    pub fn validate(text: &str, root: &Path) -> Result<()> {
        let file: spec::File = toml::from_str(text).context("the file is not valid TOML")?;
        Self::assemble(
            root.to_path_buf(),
            root.join("thetis.toml"),
            file,
            Env::Process,
        )?;
        Ok(())
    }

    fn assemble(
        root: PathBuf,
        config_path: PathBuf,
        file: spec::File,
        env: Env,
    ) -> Result<Self> {
        // A worker's config is rooted at its worktree, but some paths name
        // state the whole fleet shares. The gateway pins those over the
        // environment at spawn, so a branch cannot retarget them by editing
        // its own copy of thetis.toml.
        let shared = |env_key: &str, fallback: PathBuf| -> PathBuf {
            env.string(env_key).map(PathBuf::from).unwrap_or(fallback)
        };
        let paths = Paths {
            data: shared("THETIS_DATA_DIR", resolve(&root, &file.paths.data)),
            artifacts: shared(
                "THETIS_ARTIFACTS_DIR",
                resolve(&root, &file.paths.artifacts),
            ),
            skills: resolve(&root, &file.paths.skills),
            templates: resolve(&root, &file.paths.templates),
            wit: resolve(&root, &file.paths.wit),
            agent: resolve(&root, &file.paths.agent),
            gateways: resolve(&root, &file.paths.gateways),
            gateway_prefix: file.paths.gateway_prefix,
            tools: resolve(&root, &file.paths.tools),
            tool_prefix: file.paths.tool_prefix,
            worktrees: resolve(&root, &file.paths.worktrees),
        };

        // The WASI preopens, resolved once: they are needed twice over, because
        // the shared workspace is both what guests get as `/workspace` and
        // something the host-side file tools must be able to reach. A worker's
        // gateway pins the real directory over the environment, so a branch
        // cannot point its own workspace somewhere private.
        let wasi_dirs: Vec<PathBuf> = match env.string("THETIS_WORKSPACE_DIR") {
            Some(dir) => vec![PathBuf::from(dir)],
            None => file.wasi.dirs.iter().map(|d| resolve(&root, d)).collect(),
        };

        let bind_raw = env.string("THETIS_BIND").unwrap_or(file.server.bind);
        let bind_addr: SocketAddr = bind_raw
            .parse()
            .with_context(|| format!("`{bind_raw}` is not a valid host:port"))?;

        // What the agent calls itself, as distinct from the harness running it.
        let agent_name = resolve_agent_name(env.string("THETIS_AGENT_NAME"), &file.agent.name);
        let agent_avatar = env
            .string("THETIS_AGENT_AVATAR")
            .unwrap_or(file.agent.avatar)
            .trim()
            .to_string();

        // A prompt file wins over an inline prompt; neither means the built-in.
        let system_prompt = match env.string("THETIS_SYSTEM_PROMPT") {
            Some(p) => p,
            None if !file.agent.system_prompt_file.is_empty() => {
                let path = resolve(&root, &file.agent.system_prompt_file);
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading the system prompt at {}", path.display()))?
            }
            None if !file.agent.system_prompt.is_empty() => file.agent.system_prompt,
            None => default_system_prompt().to_string(),
        };
        // `{agent_name}` is substituted wherever it appears, in a custom prompt
        // as much as in the built-in one, so renaming the agent does not mean
        // hand-editing a prompt file as well.
        let system_prompt = system_prompt.replace(AGENT_NAME_PLACEHOLDER, &agent_name);

        let models = match env.string("THETIS_MODELS") {
            Some(raw) => parse_models_env(&raw),
            None if !file.models.is_empty() => file
                .models
                .into_iter()
                .map(|m| ModelSpec {
                    label: if m.label.is_empty() { m.id.clone() } else { m.label },
                    id: m.id,
                    provider: m.provider,
                    wire_model: m.wire_model,
                })
                .collect(),
            None => builtin_models(),
        };

        // `[llm]` is always a provider, so every existing config keeps working
        // and the `[[providers]]` list is purely additive.
        let llm_api_key = resolve_api_key(env.string("OPENROUTER_API_KEY"), &file.llm.api_key);
        let llm_base = env.string("OPENROUTER_BASE_URL").unwrap_or(file.llm.base_url);
        let mut providers = vec![ProviderSpec {
            id: DEFAULT_PROVIDER_ID.to_string(),
            label: "OpenRouter".into(),
            base_urls: vec![llm_base.clone()],
            api_key: llm_api_key.clone(),
            headers: Vec::new(),
        }];
        for p in file.providers {
            let id = p.id.trim().to_string();
            if id.is_empty() {
                anyhow::bail!("a [[providers]] entry has no id");
            }
            // `base_url` and `base_urls` are the same field, one endpoint or
            // several. Accept either spelling and normalize to the list.
            let mut base_urls: Vec<String> = Vec::new();
            if !p.base_url.trim().is_empty() {
                base_urls.push(p.base_url.trim().to_string());
            }
            for url in &p.base_urls {
                let url = url.trim();
                if url.is_empty() {
                    anyhow::bail!("provider `{id}` has an empty entry in base_urls");
                }
                if !base_urls.iter().any(|existing| existing == url) {
                    base_urls.push(url.to_string());
                }
            }
            if base_urls.is_empty() {
                anyhow::bail!("provider `{id}` has no base_url");
            }
            let api_key = resolve_provider_key(env, &p.api_key);
            let spec = ProviderSpec {
                label: if p.label.is_empty() { id.clone() } else { p.label },
                id: id.clone(),
                base_urls,
                api_key,
                headers: p.headers.into_iter().collect(),
            };
            // A [[providers]] entry named `openrouter` replaces the synthesized
            // one rather than sitting unreachable behind it.
            match providers.iter_mut().find(|existing| existing.id == id) {
                Some(existing) => *existing = spec,
                None => providers.push(spec),
            }
        }

        let default_provider = env
            .string("THETIS_PROVIDER")
            .unwrap_or(file.llm.provider)
            .trim()
            .to_string();
        let default_provider = if default_provider.is_empty() {
            DEFAULT_PROVIDER_ID.to_string()
        } else {
            if !providers.iter().any(|p| p.id == default_provider) {
                anyhow::bail!(
                    "llm.provider `{default_provider}` is not one of the configured providers ({})",
                    providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
            default_provider
        };

        // A model naming a provider that does not exist would only fail at
        // request time, long after the mistake was made.
        for m in &models {
            if !m.provider.is_empty() && !providers.iter().any(|p| p.id == m.provider) {
                anyhow::bail!(
                    "model `{}` names provider `{}`, which is not configured ({})",
                    m.id,
                    m.provider,
                    providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }

        let mut modes: Vec<ModeSpec> = if file.modes.is_empty() {
            builtin_modes()
        } else {
            file.modes
                .into_iter()
                .map(|m| ModeSpec {
                    label: if m.label.is_empty() { m.id.clone() } else { m.label },
                    id: m.id,
                    description: m.description,
                    read_only: m.read_only,
                    prompt: m.prompt,
                })
                .collect()
        };
        // A mode prompt is appended to the system prompt, so it gets the same
        // substitution — otherwise `{agent_name}` would reach the model raw.
        for mode in &mut modes {
            mode.prompt = mode.prompt.replace(AGENT_NAME_PLACEHOLDER, &agent_name);
        }

        let default_mode = env.string("THETIS_DEFAULT_MODE").unwrap_or(file.agent.default_mode);
        if !modes.iter().any(|m| m.id == default_mode) {
            anyhow::bail!(
                "default_mode `{default_mode}` is not one of the configured modes ({})",
                modes
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let config = Self {
            paths,
            bind_addr,
            primary_gateway: env.string("THETIS_GATEWAY")
                .unwrap_or(file.server.primary_gateway),
            admin_enabled: env.parse("THETIS_ADMIN", file.server.admin_enabled),

            openrouter_api_key: llm_api_key,
            openrouter_base: llm_base,
            model: env.string("THETIS_MODEL").unwrap_or(file.llm.model),
            request_timeout: Duration::from_secs(env.parse(
                "THETIS_REQUEST_TIMEOUT_SECS",
                file.llm.request_timeout_secs,
            )),
            max_retries: env.parse("THETIS_MAX_RETRIES", file.llm.max_retries),
            models,
            providers,
            default_provider,

            agent_name,
            agent_avatar,
            system_prompt,
            max_iterations: env.parse("THETIS_MAX_ITERATIONS", file.agent.max_iterations),
            modes,
            default_mode,

            wasm_slice: Duration::from_secs(env.parse(
                "THETIS_WASM_SLICE_SECS",
                file.budgets.wasm_slice_secs,
            )),
            tool_budget: Duration::from_secs(env.parse(
                "THETIS_TOOL_BUDGET_SECS",
                file.budgets.tool_secs,
            )),
            probe_budget: Duration::from_secs(env.parse(
                "THETIS_PROBE_BUDGET_SECS",
                file.budgets.probe_secs,
            )),

            agent_memory_bytes: env.parse("THETIS_AGENT_MEM_MB", file.limits.agent_memory_mb)
                << 20,
            tool_memory_bytes: env.parse("THETIS_TOOL_MEM_MB", file.limits.tool_memory_mb) << 20,
            gateway_memory_bytes: env.parse(
                "THETIS_GATEWAY_MEM_MB",
                file.limits.gateway_memory_mb,
            ) << 20,
            session_spend_limit_usd: env.parse(
                "THETIS_SESSION_SPEND_LIMIT_USD",
                file.limits.session_spend_limit_usd,
            ),
            max_tool_output_bytes: env.parse(
                "THETIS_MAX_TOOL_OUTPUT",
                file.limits.max_tool_output_bytes,
            ),
            max_attachment_bytes: env.parse(
                "THETIS_MAX_ATTACHMENT_BYTES",
                file.limits.max_attachment_bytes,
            ),
            max_attachments: env.parse("THETIS_MAX_ATTACHMENTS", file.limits.max_attachments),

            context: ContextSettings {
                enabled: env.parse("THETIS_COMPACT", file.context.enabled),
                window: env.parse("THETIS_CONTEXT_WINDOW", file.context.window_tokens).max(1),
                compact_threshold: file.context.compact_threshold.clamp(0.05, 0.99),
                compact_target: file.context.compact_target.clamp(0.01, 0.95),
                summary_model: env.string("THETIS_SUMMARY_MODEL")
                    .unwrap_or(file.context.summary_model),
                keep_head: file.context.keep_head,
                keep_tail: file.context.keep_tail,
            },

            cache: CacheSettings {
                enabled: env.parse("THETIS_CACHE", file.cache.enabled),
                ttl: env.string("THETIS_CACHE_TTL").unwrap_or(file.cache.ttl),
                anchor_stride: file.cache.anchor_stride.max(1),
                explicit_vendors: file.cache.explicit_vendors,
            },

            skills: SkillSettings {
                retrieval_enabled: env.parse(
                    "THETIS_SKILL_RETRIEVAL",
                    file.skills.retrieval_enabled,
                ),
                // A limit of zero would disable the L1 block by accident; use
                // `retrieval_enabled = false` to mean that on purpose.
                retrieve_limit: file.skills.retrieve_limit.clamp(1, 50),
                embedding_model: env.string("THETIS_EMBEDDING_MODEL")
                    .unwrap_or(file.skills.embedding_model),
                embedding_provider: env.string("THETIS_EMBEDDING_PROVIDER")
                    .unwrap_or(file.skills.embedding_provider),
                embedding_dimensions: file.skills.embedding_dimensions.clamp(64, 4096),
                max_query_chars: file.skills.max_query_chars.clamp(64, 32_768),
                max_universal: file.skills.max_universal.min(20),
            },

            tool_groups: ToolGroupSettings {
                grouping_enabled: env.parse(
                    "THETIS_TOOL_GROUPING",
                    file.tool_groups.grouping_enabled,
                ),
                accounting_enabled: file.tool_groups.accounting_enabled,
                always_on: file.tool_groups.always_on,
                route_threshold: file.tool_groups.route_threshold.clamp(0.0, 1.0),
            },

            build: BuildSettings {
                command: env.string("THETIS_BUILD_COMMAND").unwrap_or(file.build.command),
                target: env.string("THETIS_BUILD_TARGET").unwrap_or(file.build.target),
                profile: env.string("THETIS_BUILD_PROFILE").unwrap_or(file.build.profile),
                target_dir: shared("THETIS_TARGET_DIR", resolve(&root, &file.build.target_dir)),
                locked: file.build.locked,
                extra_args: file.build.extra_args,
                timeout: Duration::from_secs(
                    env.parse("THETIS_BUILD_TIMEOUT_SECS", file.build.timeout_secs).max(1),
                ),
                allowed_crates: file.build.allowed_crates,
            },

            wasi: WasiSettings {
                network: env.parse("THETIS_WASI_NETWORK", file.wasi.network),
                dns: env.parse("THETIS_WASI_DNS", file.wasi.dns),
                env: env.parse("THETIS_WASI_ENV", file.wasi.env),
                stdio: env.parse("THETIS_WASI_STDIO", file.wasi.stdio),
                dirs: wasi_dirs.clone(),
            },

            watchdog: WatchdogSettings {
                failure_window: Duration::from_secs(file.watchdog.failure_window_secs),
                failure_threshold: file.watchdog.failure_threshold.max(1),
                probe_interval: Duration::from_secs(file.watchdog.probe_interval_secs.max(1)),
                watch_suppression: Duration::from_secs(file.watchdog.watch_suppression_secs),
                debounce: Duration::from_millis(file.watchdog.debounce_ms),
            },

            devkit: DevkitSettings {
                enabled: env.parse("THETIS_DEVKIT", file.devkit.enabled),
                protected_files: file.devkit.protected_files,
                protected_dirs: file.devkit.protected_dirs,
            },

            filesystem: FilesystemSettings {
                enabled: env.parse("THETIS_FILESYSTEM", file.filesystem.enabled),
                roots: {
                    let mut roots: Vec<PathBuf> = if file.filesystem.roots.is_empty() {
                        vec![root.clone()]
                    } else {
                        file.filesystem
                            .roots
                            .iter()
                            .map(|r| resolve(&root, r))
                            .collect()
                    };
                    // The shared workspace is always reachable. It is the one
                    // directory every guest already has as a WASI preopen and
                    // every conversation and branch shares, so having the host
                    // file tools refuse it was incoherent: an agent could write
                    // there through a tool component but not read it back with
                    // `read_path`, and in a mode where the terminal is withheld
                    // it could not reach it at all. Granting it here takes away
                    // no confinement that was doing work — the authority was
                    // already handed out at the preopen — and it is appended
                    // rather than prepended so relative paths still resolve
                    // against the project root.
                    for dir in &wasi_dirs {
                        if !roots.iter().any(|r| r == dir) {
                            roots.push(dir.clone());
                        }
                    }
                    roots
                },
                max_read_bytes: file.filesystem.max_read_bytes,
                protected: file.filesystem.protected,
                allow_delete: file.filesystem.allow_delete,
            },

            terminal: TerminalSettings {
                enabled: env.parse("THETIS_TERMINAL", file.terminal.enabled),
                shell: if file.terminal.shell.is_empty() {
                    default_shell().to_string()
                } else {
                    file.terminal.shell
                },
                shell_args: if file.terminal.shell_args.is_empty() {
                    default_shell_args()
                } else {
                    file.terminal.shell_args
                },
                max_sessions: file.terminal.max_sessions.max(1),
                default_timeout: Duration::from_millis(file.terminal.default_timeout_ms),
                max_output_bytes: file.terminal.max_output_bytes,
                idle_timeout: Duration::from_secs(file.terminal.idle_timeout_secs),
                ssh_enabled: env.parse("THETIS_TERMINAL_SSH", file.terminal.ssh_enabled),
                ssh_program: if file.terminal.ssh_program.is_empty() {
                    "ssh".into()
                } else {
                    file.terminal.ssh_program
                },
                ssh_connect_timeout: Duration::from_millis(
                    file.terminal.ssh_connect_timeout_ms.max(1_000),
                ),
                send_settle: Duration::from_millis(file.terminal.send_settle_ms),
            },

            control: ControlSettings {
                allow_restart: env.parse("THETIS_ALLOW_RESTART", file.control.allow_restart),
                min_uptime: Duration::from_secs(file.control.min_uptime_secs),
                restart_delay: Duration::from_millis(file.control.restart_delay_ms),
            },

            discord: DiscordSettings {
                enabled: env.parse("DISCORD_ENABLED", file.discord.enabled),
                // The environment wins, so the token need never be on disk.
                bot_token: env.string("DISCORD_BOT_TOKEN")
                    .or_else(|| Some(file.discord.bot_token.clone()).filter(|t| !t.trim().is_empty()))
                    .map(Secret::new),
                mode: env.string("DISCORD_MODE").unwrap_or(file.discord.mode),
                allowed_users: env.list("DISCORD_ALLOWED_USERS")
                    .unwrap_or(file.discord.allowed_users),
                admin_users: env.list("DISCORD_ADMIN_USERS")
                    .unwrap_or(file.discord.admin_users),
                allow_all_users: env.parse("DISCORD_ALLOW_ALL_USERS", file.discord.allow_all_users),
                require_mention: env.parse("DISCORD_REQUIRE_MENTION", file.discord.require_mention),
                free_response_channels: env.list("DISCORD_FREE_RESPONSE_CHANNELS")
                    .unwrap_or(file.discord.free_response_channels),
                ignore_no_mention: env.parse(
                    "DISCORD_IGNORE_NO_MENTION",
                    file.discord.ignore_no_mention,
                ),
                group_sessions_per_user: env.parse(
                    "DISCORD_GROUP_SESSIONS_PER_USER",
                    file.discord.group_sessions_per_user,
                ),
                stream_edit_interval: Duration::from_millis(
                    file.discord.stream_edit_interval_ms.max(250),
                ),
                pairing_code_ttl: Duration::from_secs(
                    file.discord.pairing_code_ttl_secs.max(30),
                ),
            },

            browser: BrowserSettings {
                enabled: env.parse("THETIS_BROWSER_ENABLED", file.browser.enabled),
                port: env.parse("THETIS_BROWSER_PORT", file.browser.port),
                service_dir: resolve(&root, &file.browser.service_dir),
                node: env.string("THETIS_BROWSER_NODE").unwrap_or(file.browser.node),
                npm: env.string("THETIS_BROWSER_NPM").unwrap_or(file.browser.npm),
                playwright_version: env
                    .string("THETIS_BROWSER_PLAYWRIGHT_VERSION")
                    .unwrap_or(file.browser.playwright_version),
                auto_install: env.parse(
                    "THETIS_BROWSER_AUTO_INSTALL",
                    file.browser.auto_install,
                ),
                install_timeout: Duration::from_secs(
                    file.browser.install_timeout_secs.max(30),
                ),
                startup_timeout: Duration::from_secs(
                    file.browser.startup_timeout_secs.max(5),
                ),
                default_timeout_ms: file.browser.default_timeout_ms.max(1_000),
                idle_timeout_secs: file.browser.idle_timeout_secs,
                snapshot_chars: file.browser.snapshot_chars.max(500),
                artifact_dir: resolve(&root, &file.browser.artifact_dir),
            },

            sandbox_available: env.parse("THETIS_SANDBOX", file.sandbox.enabled),
            tools: file.tools,
            config_path,
            root,
        };

        tracing::debug!(
            config = %config.config_path.display(),
            exists = config.config_path.is_file(),
            "configuration loaded"
        );
        Ok(config)
    }
}

/// The environment wins, so a key can be overridden for one run without editing
/// the file. Blank in either place counts as absent: an empty string would
/// otherwise become an `Authorization: Bearer ` header and fail confusingly at
/// request time rather than at startup.
fn resolve_api_key(from_env: Option<String>, from_file: &str) -> Option<Secret> {
    from_env
        .or_else(|| Some(from_file.to_string()))
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .map(Secret::new)
}

/// A provider's key, with `env:NAME` indirection.
///
/// Empty stays empty and means *send no auth header*, which is what a local
/// llama.cpp server wants. `env:NAME` that is unset is also empty rather than
/// an error: an unauthenticated endpoint is a perfectly good outcome, and a
/// wrong key fails loudly at the first request anyway.
fn resolve_provider_key(env: Env, raw: &str) -> Option<Secret> {
    let raw = raw.trim();
    let value = match raw.strip_prefix("env:") {
        Some(name) => env.string(name.trim()).unwrap_or_default(),
        None => raw.to_string(),
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(Secret::new(value))
    }
}

/// `THETIS_MODELS` is a comma-separated list of `id=Label` pairs; a bare id
/// uses itself as the label.
fn parse_models_env(raw: &str) -> Vec<ModelSpec> {
    let parsed: Vec<ModelSpec> = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (id, label) = match entry.split_once('=') {
                Some((id, label)) => (id.trim().to_string(), label.trim().to_string()),
                None => (entry.to_string(), entry.to_string()),
            };
            ModelSpec {
                id,
                label,
                // An env-declared model routes by id prefix, if at all.
                provider: String::new(),
                wire_model: String::new(),
            }
        })
        .collect();

    if parsed.is_empty() {
        builtin_models()
    } else {
        parsed
    }
}

/// A short starting list. Override it in `thetis.toml` — a provider's
/// catalogue changes far faster than this file does.
fn builtin_models() -> Vec<ModelSpec> {
    [
        ("anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5"),
        ("anthropic/claude-opus-4.1", "Claude Opus 4.1"),
        ("openai/gpt-4o", "GPT-4o"),
        ("google/gemini-2.5-pro", "Gemini 2.5 Pro"),
        ("mock/echo", "Mock (local test server)"),
    ]
    .into_iter()
    .map(|(id, label)| ModelSpec {
        id: id.to_string(),
        label: label.to_string(),
        provider: String::new(),
        wire_model: String::new(),
    })
    .collect()
}

/// PowerShell on Windows, a POSIX shell elsewhere.
fn default_shell() -> &'static str {
    if cfg!(windows) {
        "powershell"
    } else {
        "sh"
    }
}

fn default_shell_args() -> Vec<String> {
    if cfg!(windows) {
        // No profile and no banner: a predictable, quiet session.
        ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"]
            .map(String::from)
            .to_vec()
    } else {
        vec!["-s".to_string()]
    }
}

/// What Plan mode tells the model it is for.
///
/// It lives with the mode that uses it rather than in the base system prompt,
/// which has to stay true in every mode.
const PLAN_PROMPT: &str = "You are in Plan mode. Every tool that would change something is withheld, so you cannot edit code, run commands, or modify the running system. Read and reason only.\n\nProduce a plan, not an implementation:\n- Investigate first with the read-only tools you do have. A plan written without looking at the code is a guess.\n- Say what you would change, file by file, and why each change is needed.\n- Name the decisions that are the user's to make, and the risks worth checking before starting.\n- Sketch the shape of code rather than writing it out in full. You cannot compile it here, and untested code reads as more certain than it is.\n\nIf you find yourself wanting a tool that is not offered, say what you would have done with it instead of quietly working around it. When the plan is ready, say so plainly and stop; the user will switch to Agent mode to carry it out.";

fn builtin_modes() -> Vec<ModeSpec> {
    vec![
        ModeSpec {
            id: "agent".into(),
            label: "Agent".into(),
            description: "Full access. Runs tools and can modify the running system.".into(),
            read_only: false,
            prompt: String::new(),
        },
        ModeSpec {
            id: "plan".into(),
            label: "Plan".into(),
            description:
                "Reads and reasons, but makes no changes. Tools that would modify anything are withheld."
                    .into(),
            read_only: true,
            prompt: PLAN_PROMPT.into(),
        },
    ]
}

/// What the agent calls itself when nothing says otherwise.
pub const DEFAULT_AGENT_NAME: &str = "Thetis";

/// Picks the agent's name: the environment, then the file, then the default.
///
/// Split out as a pure function so the precedence can be tested without
/// mutating the process environment, which is shared by every test in the
/// binary and by the live conversations this process is serving.
///
/// Blank or whitespace at either layer means "not set" rather than "no name":
/// an empty name would produce `You are , an agent...` and read as a bug in the
/// harness rather than a mistake in the config.
fn resolve_agent_name(from_env: Option<String>, from_file: &str) -> String {
    from_env
        .filter(|n| !n.trim().is_empty())
        .or_else(|| Some(from_file.to_string()).filter(|n| !n.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string())
        .trim()
        .to_string()
}

/// Substituted for the configured agent name anywhere in a system or mode
/// prompt. A custom prompt gets this too, so a rename reaches it without the
/// author having to hardcode the name.
pub const AGENT_NAME_PLACEHOLDER: &str = "{agent_name}";

fn default_system_prompt() -> &'static str {
    "You are {agent_name}, an agent running inside a self-modifying WebAssembly grip named Thetis.

You are unusual: your own agentic loop, your tools, and the chat interface you \
are speaking through are all WebAssembly components that you can rewrite while \
you run. Edits are compiled immediately and the compiler's verdict comes back \
in the same tool result, so iterate until it builds. Every component is \
versioned and can be rolled back, so a broken build is recoverable, never fatal.

Work through the file tools rather than the shell. `search_files` finds where \
something lives, `find_files` finds a file by name, `read_path` reads it with \
line numbers, and `edit_path` changes part of it in place. They cost a \
fraction of the tokens that grep, sed and heredocs do, they fail in ways you \
can act on, and an edit made through them is recorded as yours. Keep the \
terminal for running things — builds, tests, git, processes — which is what it \
is good at.

Be direct and concise. Use tools when they help; explain what you changed when \
you modify yourself."
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a config from a TOML string alone.
    ///
    /// `Env::None` is the point: these tests assert on what the file says, and
    /// a `THETIS_*` variable in the ambient environment would otherwise
    /// override the very value under assertion. Thetis runs its own suite
    /// inside a worker, where the gateway has pinned several of them.
    fn from_toml(text: &str) -> Result<Config> {
        Config::assemble(
            PathBuf::from("/proj"),
            PathBuf::from("/proj/thetis.toml"),
            toml::from_str(text)?,
            Env::None,
        )
    }

    #[test]
    fn runs_with_no_configuration_at_all() {
        let cfg = from_toml("").unwrap();
        assert_eq!(cfg.bind_addr.port(), 7777);
        assert_eq!(cfg.build.target, "wasm32-wasip2");
        assert_eq!(cfg.modes.len(), 2);
        assert!(cfg.mode("plan").unwrap().read_only);
        assert!(!cfg.mode("agent").unwrap().read_only);
    }

    /// The shared workspace is every agent's common ground and is handed to
    /// every guest as a preopen, so the host file tools must be able to reach
    /// it whatever `filesystem.roots` says — including when a deployment sets
    /// roots explicitly and forgets it.
    #[test]
    fn the_workspace_is_always_a_filesystem_root() {
        let cfg = from_toml("").unwrap();
        let ws = cfg.wasi.dirs[0].clone();
        assert_eq!(ws, PathBuf::from("/proj/workspace"));
        assert!(cfg.filesystem.roots.contains(&ws), "{:?}", cfg.filesystem.roots);

        // Explicit roots that omit the workspace still get it.
        let cfg = from_toml("[filesystem]\nroots = [\"/elsewhere\"]\n").unwrap();
        assert_eq!(cfg.filesystem.roots[0], PathBuf::from("/elsewhere"));
        assert!(
            cfg.filesystem.roots.contains(&PathBuf::from("/proj/workspace")),
            "{:?}",
            cfg.filesystem.roots
        );

        // Relative paths still resolve against the project root, not the
        // workspace: the workspace is appended, never prepended.
        let cfg = from_toml("").unwrap();
        assert_eq!(cfg.filesystem.roots[0], PathBuf::from("/proj"));

        // And it is not duplicated when it is already named.
        let cfg = from_toml("[filesystem]\nroots = [\"workspace\"]\n").unwrap();
        assert_eq!(
            cfg.filesystem
                .roots
                .iter()
                .filter(|r| **r == PathBuf::from("/proj/workspace"))
                .count(),
            1,
            "{:?}",
            cfg.filesystem.roots
        );
    }

    /// The agent's name defaults to the harness's, which is what made the two
    /// easy to conflate in the first place.
    #[test]
    fn the_agent_is_named_thetis_by_default() {
        let cfg = from_toml("").unwrap();
        assert_eq!(cfg.agent_name, "Thetis");
        assert!(
            cfg.system_prompt.starts_with("You are Thetis,"),
            "the placeholder should have been substituted, got: {:?}",
            &cfg.system_prompt[..40.min(cfg.system_prompt.len())]
        );
    }

    #[test]
    fn naming_the_agent_renames_it_in_the_built_in_prompt() {
        let cfg = from_toml(
            r#"
            [agent]
            name = "Ada"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.agent_name, "Ada");
        assert!(cfg.system_prompt.starts_with("You are Ada,"));
        // The harness keeps its own name in the same sentence.
        assert!(cfg.system_prompt.contains("grip named Thetis"));
    }

    /// The point of the placeholder: a custom prompt should not have to
    /// hardcode the name to follow a rename.
    #[test]
    fn a_custom_prompt_gets_the_placeholder_substituted() {
        let cfg = from_toml(
            r#"
            [agent]
            name = "Ada"
            system_prompt = "You are {agent_name}. Greet as {agent_name}."
            "#,
        )
        .unwrap();
        assert_eq!(cfg.system_prompt, "You are Ada. Greet as Ada.");
        assert!(
            !cfg.system_prompt.contains("{agent_name}"),
            "no placeholder may reach the model raw"
        );
    }

    #[test]
    fn a_mode_prompt_gets_the_placeholder_too() {
        let cfg = from_toml(
            r#"
            [agent]
            name = "Ada"

            [[modes]]
            id = "agent"
            label = "Agent"
            prompt = "You are {agent_name} in agent mode."
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mode("agent").unwrap().prompt, "You are Ada in agent mode.");
    }

    /// Scoping must default to off. An empty config that silently narrowed the
    /// tool surface would change every existing deployment's behaviour on
    /// upgrade, which is exactly what the research says not to do blind.
    #[test]
    fn tool_grouping_is_off_by_default_but_accounting_is_on() {
        let cfg = from_toml("").unwrap();
        assert!(!cfg.tool_groups.grouping_enabled);
        assert!(
            cfg.tool_groups.accounting_enabled,
            "the baseline measurement is what makes enabling grouping judgeable"
        );
        assert!(cfg.tool_groups.always_on.is_empty());
        assert_eq!(cfg.tool_groups.route_threshold, 0.15);
    }

    #[test]
    fn tool_group_settings_round_trip_from_toml() {
        let cfg = from_toml(
            r#"
            [tool_groups]
            grouping_enabled = true
            accounting_enabled = false
            always_on = ["shell", "web"]
            route_threshold = 0.4
            "#,
        )
        .unwrap();
        assert!(cfg.tool_groups.grouping_enabled);
        assert!(!cfg.tool_groups.accounting_enabled);
        assert_eq!(cfg.tool_groups.always_on, vec!["shell", "web"]);
        assert_eq!(cfg.tool_groups.route_threshold, 0.4);
    }

    /// A threshold above 1.0 would admit nothing on tag evidence and a negative
    /// one would admit everything; both are silent misconfigurations, so they
    /// are clamped rather than trusted.
    #[test]
    fn an_out_of_range_route_threshold_is_clamped() {
        let high = from_toml("[tool_groups]\nroute_threshold = 9.0\n").unwrap();
        assert_eq!(high.tool_groups.route_threshold, 1.0);
        let low = from_toml("[tool_groups]\nroute_threshold = -3.0\n").unwrap();
        assert_eq!(low.tool_groups.route_threshold, 0.0);
    }

    #[test]
    fn the_agent_has_no_avatar_unless_one_is_configured() {
        // Empty is meaningful: it selects the built-in mark.
        assert_eq!(from_toml("").unwrap().agent_avatar, "");
    }

    #[test]
    fn an_avatar_can_be_a_url_or_a_data_uri() {
        let cfg = from_toml(
            r#"
            [agent]
            avatar = "  https://example.com/ada.png  "
            "#,
        )
        .unwrap();
        // Trimmed, because stray whitespace in an attribute value is a broken
        // image rather than a helpful error.
        assert_eq!(cfg.agent_avatar, "https://example.com/ada.png");

        let cfg = from_toml(
            r#"
            [agent]
            avatar = "data:image/png;base64,iVBORw0KGgo="
            "#,
        )
        .unwrap();
        assert_eq!(cfg.agent_avatar, "data:image/png;base64,iVBORw0KGgo=");
    }

    /// An empty or whitespace name is a mistake, not a request for a nameless
    /// agent: falling through to the default keeps the prompt grammatical.
    #[test]
    fn a_blank_name_falls_back_to_the_default() {
        let cfg = from_toml(
            r#"
            [agent]
            name = "   "
            "#,
        )
        .unwrap();
        assert_eq!(cfg.agent_name, "Thetis");
    }

    #[test]
    fn the_environment_names_the_agent_over_the_file() {
        // Tested through the pure resolver rather than by setting a process
        // variable: this binary's tests share one environment, and so do the
        // conversations this process is serving.
        assert_eq!(resolve_agent_name(Some("Ada".into()), "Grace"), "Ada");
        assert_eq!(resolve_agent_name(None, "Grace"), "Grace");
        assert_eq!(resolve_agent_name(None, ""), "Thetis");

        // Blank at either layer defers instead of yielding a nameless agent.
        assert_eq!(resolve_agent_name(Some("  ".into()), "Grace"), "Grace");
        assert_eq!(resolve_agent_name(Some("  ".into()), "  "), "Thetis");

        // Whitespace from a copy-paste is trimmed, not carried into the prompt.
        assert_eq!(resolve_agent_name(None, "  Ada  "), "Ada");
    }

    #[test]
    fn a_partial_file_only_overrides_what_it_names() {
        let cfg = from_toml(
            r#"
            [server]
            bind = "0.0.0.0:9000"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.bind_addr.port(), 9000);
        // Untouched sections keep their defaults.
        assert_eq!(cfg.max_iterations, 32);
        assert_eq!(cfg.watchdog.failure_threshold, 3);
    }

    #[test]
    fn a_mistyped_key_is_reported_but_does_not_stop_startup() {
        let text = r#"
            [budgets]
            turn_seconds = 60
            "#;

        // It still loads: a config the running binary does not fully understand
        // must not be able to keep the process down, because the process is what
        // rebuilds the binary that would understand it.
        let cfg = from_toml(text).expect("a stray key should not be fatal");
        assert_eq!(cfg.wasm_slice.as_secs(), 10, "defaults still apply");

        // And it is not silent: the key comes back named.
        let value: toml::Value = toml::from_str(text).unwrap();
        let (_, unknown) = parse_file(value).unwrap();
        assert!(
            unknown.iter().any(|k| k.contains("turn_seconds")),
            "{unknown:?}"
        );
    }

    #[test]
    fn a_whole_unknown_section_is_reported_rather_than_fatal() {
        // The shape that actually took the service down: config gained a section
        // before the binary that understands it was built.
        let text = "[telemetry]
endpoint = 0
";
        let value: toml::Value = toml::from_str(text).unwrap();
        let (_, unknown) = parse_file(value).unwrap();
        assert!(unknown.iter().any(|k| k.contains("telemetry")), "{unknown:?}");
    }

    #[test]
    fn the_skills_section_is_understood_rather_than_reported_unknown() {
        // The counterpart to the test above. `[skills]` was once its example of
        // an unknown section; it is a real one now, so this pins the other side
        // of that line and would catch the section being silently dropped.
        let text = r#"
[skills]
retrieval_enabled = false
retrieve_limit = 7
embedding_model = "test/model"
embedding_dimensions = 256
max_query_chars = 99
max_universal = 3
"#;
        let value: toml::Value = toml::from_str(text).unwrap();
        let (file, unknown) = parse_file(value).unwrap();
        assert!(
            !unknown.iter().any(|k| k.contains("skills")),
            "skills should be understood now: {unknown:?}"
        );
        assert!(!file.skills.retrieval_enabled);
        assert_eq!(file.skills.retrieve_limit, 7);
        assert_eq!(file.skills.embedding_model, "test/model");
        assert_eq!(file.skills.embedding_dimensions, 256);
        assert_eq!(file.skills.max_query_chars, 99);
        assert_eq!(file.skills.max_universal, 3);
    }

    /// The config a llama.cpp user would write: one extra provider, one model
    /// pointed at it, OpenRouter left exactly as it was.
    const LOCAL_PROVIDER: &str = r#"
        [llm]
        model = "anthropic/claude-sonnet-4.5"
        api_key = "sk-or-test"

        [[providers]]
        id = "local"
        label = "llama.cpp"
        base_url = "http://127.0.0.1:8080/v1"

        [[models]]
        id = "anthropic/claude-sonnet-4.5"
        label = "Claude Sonnet 4.5"

        [[models]]
        id = "local/qwen3-30b"
        label = "Qwen3 30B (local)"
        provider = "local"
        wire_model = "qwen3-30b-a3b"
    "#;

    #[test]
    fn openrouter_is_always_a_provider_even_with_none_configured() {
        let cfg = from_toml("[llm]\napi_key = \"sk-or-test\"\n").unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.default_provider, DEFAULT_PROVIDER_ID);

        let provider = cfg.fallback_provider();
        assert_eq!(provider.base_url(), "https://openrouter.ai/api/v1");
        assert!(provider.is_openrouter());
        assert_eq!(provider.api_key.as_ref().unwrap().expose(), "sk-or-test");
    }

    #[test]
    fn a_local_provider_serves_the_model_that_names_it() {
        let cfg = from_toml(LOCAL_PROVIDER).unwrap();

        let local = cfg.resolve_model("local/qwen3-30b");
        assert_eq!(local.provider.id, "local");
        assert_eq!(local.provider.url("chat/completions"), "http://127.0.0.1:8080/v1/chat/completions");
        // The picker's id and the name on the wire are allowed to differ.
        assert_eq!(local.wire_model, "qwen3-30b-a3b");
        // No key configured means no Authorization header at all, which is what
        // an unauthenticated local server needs.
        assert!(local.provider.api_key.is_none());
        assert!(!local.provider.is_openrouter());

        // Adding a provider must not move anything that was already working.
        let remote = cfg.resolve_model("anthropic/claude-sonnet-4.5");
        assert_eq!(remote.provider.id, DEFAULT_PROVIDER_ID);
        assert_eq!(remote.wire_model, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn an_unlisted_model_can_still_address_a_provider_by_prefix() {
        let cfg = from_toml(LOCAL_PROVIDER).unwrap();
        let resolved = cfg.resolve_model("local/some-gguf-i-just-loaded");
        assert_eq!(resolved.provider.id, "local");
        // The prefix is the routing instruction, so it comes off the wire name.
        assert_eq!(resolved.wire_model, "some-gguf-i-just-loaded");
    }

    #[test]
    fn a_vendor_prefix_is_not_mistaken_for_a_provider() {
        let cfg = from_toml(LOCAL_PROVIDER).unwrap();
        // `anthropic` is a vendor within OpenRouter, not a configured provider,
        // so the id must reach the wire whole.
        let resolved = cfg.resolve_model("anthropic/claude-opus-4.1");
        assert_eq!(resolved.provider.id, DEFAULT_PROVIDER_ID);
        assert_eq!(resolved.wire_model, "anthropic/claude-opus-4.1");
    }

    #[test]
    fn a_local_provider_can_be_made_the_default() {
        let cfg = from_toml(
            r#"
            [llm]
            model = "qwen3-30b"
            provider = "local"

            [[providers]]
            id = "local"
            base_url = "http://127.0.0.1:8080/v1"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.default_provider, "local");
        let resolved = cfg.resolve_model("qwen3-30b");
        assert_eq!(resolved.provider.id, "local");
        assert_eq!(resolved.wire_model, "qwen3-30b");
        // A label falls back to the id rather than showing blank in the picker.
        assert_eq!(cfg.provider("local").unwrap().label, "local");
    }

    #[test]
    fn a_provider_key_can_come_from_the_environment() {
        // `env:` indirection keeps a real key out of the file.
        std::env::set_var("THETIS_TEST_PROVIDER_KEY", "sk-local-abc");
        let cfg = Config::assemble(
            PathBuf::from("/proj"),
            PathBuf::from("/proj/thetis.toml"),
            toml::from_str(
                r#"
                [[providers]]
                id = "vllm"
                base_url = "http://gpu.internal:8000/v1"
                api_key = "env:THETIS_TEST_PROVIDER_KEY"
                "#,
            )
            .unwrap(),
            Env::Process,
        )
        .unwrap();
        std::env::remove_var("THETIS_TEST_PROVIDER_KEY");

        assert_eq!(
            cfg.provider("vllm").unwrap().api_key.as_ref().unwrap().expose(),
            "sk-local-abc"
        );
    }

    #[test]
    fn an_unset_env_key_leaves_a_provider_unauthenticated_rather_than_failing() {
        let cfg = from_toml(
            r#"
            [[providers]]
            id = "local"
            base_url = "http://127.0.0.1:8080/v1"
            api_key = "env:DEFINITELY_NOT_SET_THETIS_TEST"
            "#,
        )
        .unwrap();
        assert!(cfg.provider("local").unwrap().api_key.is_none());
    }

    #[test]
    fn extra_headers_are_carried_per_provider() {
        let cfg = from_toml(
            r#"
            [[providers]]
            id = "gw"
            base_url = "http://gw.internal/v1"
            headers = { "X-Org" = "acme" }
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.provider("gw").unwrap().headers,
            vec![("X-Org".to_string(), "acme".to_string())]
        );
    }

    #[test]
    fn a_model_naming_a_provider_that_does_not_exist_is_rejected_at_load() {
        // Otherwise the mistake surfaces as a confusing 404 mid-conversation.
        let err = from_toml(
            r#"
            [[models]]
            id = "x"
            provider = "typo"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("typo"), "{err}");
    }

    #[test]
    fn a_default_provider_that_does_not_exist_is_rejected_at_load() {
        let err = from_toml("[llm]\nprovider = \"nope\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn a_provider_named_openrouter_replaces_the_synthesized_one() {
        // Overriding the built-in entry must not leave two under one id, where
        // the second would be unreachable.
        let cfg = from_toml(
            r#"
            [[providers]]
            id = "openrouter"
            base_url = "http://proxy.internal/v1"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.fallback_provider().base_url(), "http://proxy.internal/v1");
    }

    #[test]
    fn a_base_url_with_a_trailing_slash_does_not_double_it() {
        let cfg = from_toml(
            r#"
            [[providers]]
            id = "local"
            base_url = "http://127.0.0.1:8080/v1/"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.provider("local").unwrap().url("chat/completions"),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
    }

    #[test]
    fn modes_are_configurable_including_which_are_read_only() {
        let cfg = from_toml(
            r#"
            [agent]
            default_mode = "review"

            [[modes]]
            id = "review"
            label = "Review"
            description = "Looks, never touches."
            read_only = true

            [[modes]]
            id = "build"
            label = "Build"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.modes.len(), 2);
        assert!(cfg.mode("review").unwrap().read_only);
        assert!(!cfg.mode("build").unwrap().read_only);
        // A mode without a label falls back to its id rather than showing blank.
        assert_eq!(cfg.mode("build").unwrap().label, "Build");
    }

    #[test]
    fn a_default_mode_that_does_not_exist_is_rejected() {
        let err = from_toml(
            r#"
            [agent]
            default_mode = "nonsense"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nonsense"), "{err:#}");
    }

    #[test]
    fn paths_are_resolved_against_the_root_but_absolute_ones_are_kept() {
        let cfg = from_toml(
            r#"
            [paths]
            data = "var/state"
            artifacts = "/srv/thetis/artifacts"
            agent = "components/brain"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.paths.data, PathBuf::from("/proj/var/state"));
        assert_eq!(cfg.paths.artifacts, PathBuf::from("/srv/thetis/artifacts"));
        assert_eq!(cfg.aspect_source_dir(&Aspect::Agent), PathBuf::from("/proj/components/brain"));
        // The crate name follows the directory, so renaming it needs no code change.
        assert_eq!(cfg.aspect_crate_name(&Aspect::Agent), "brain");
        assert_eq!(cfg.aspect_wasm_filename(&Aspect::Agent), "brain.wasm");
    }

    #[test]
    fn gateway_and_tool_naming_conventions_are_configurable() {
        let cfg = from_toml(
            r#"
            [paths]
            gateways = "surfaces"
            gateway_prefix = "ui-"
            tools = "plugins"
            tool_prefix = "plugin-"
            "#,
        )
        .unwrap();

        assert_eq!(
            cfg.aspect_source_dir(&Aspect::gateway("web")),
            PathBuf::from("/proj/surfaces/ui-web")
        );
        assert_eq!(cfg.aspect_wasm_filename(&Aspect::gateway("web")), "ui_web.wasm");
        assert_eq!(
            cfg.aspect_source_dir(&Aspect::tool("weather")),
            PathBuf::from("/proj/plugins/plugin-weather")
        );
    }

    /// The regression that made Thetis unable to run its own suite: the
    /// gateway pins `THETIS_DATA_DIR` and friends into every worker, so a
    /// config test run from inside a conversation read the host's paths and
    /// failed on an assertion about the file in front of it. Green here means
    /// the suite gives the same answer wherever it runs.
    #[test]
    fn a_pinned_environment_does_not_speak_over_the_file_under_test() {
        std::env::set_var("THETIS_DATA_DIR", "/host/state");
        std::env::set_var("THETIS_ARTIFACTS_DIR", "/host/artifacts");
        std::env::set_var("THETIS_TARGET_DIR", "/host/build");

        let cfg = from_toml(
            r#"
            [paths]
            data = "var/state"
            artifacts = "var/artifacts"

            [build]
            target_dir = "build-out"
            "#,
        )
        .unwrap();

        std::env::remove_var("THETIS_DATA_DIR");
        std::env::remove_var("THETIS_ARTIFACTS_DIR");
        std::env::remove_var("THETIS_TARGET_DIR");

        assert_eq!(cfg.paths.data, PathBuf::from("/proj/var/state"));
        assert_eq!(cfg.paths.artifacts, PathBuf::from("/proj/var/artifacts"));
        assert_eq!(cfg.build.target_dir, PathBuf::from("/proj/build-out"));
    }

    /// The other half: production still honours those variables, which is how
    /// a worker is kept on the fleet's shared state rather than its worktree's.
    #[test]
    fn the_process_environment_still_overrides_when_that_is_what_is_asked_for() {
        std::env::set_var("ZZ_ENV_SOURCE_PROBE", "seen");
        assert_eq!(
            Env::Process.string("ZZ_ENV_SOURCE_PROBE"),
            Some("seen".to_string())
        );
        assert_eq!(Env::None.string("ZZ_ENV_SOURCE_PROBE"), None);
        assert_eq!(Env::Process.parse("ZZ_ENV_SOURCE_MISSING", 7), 7);
        assert_eq!(Env::None.parse("ZZ_ENV_SOURCE_PROBE", 7), 7);
        std::env::remove_var("ZZ_ENV_SOURCE_PROBE");
    }

    #[test]
    fn build_settings_flow_through() {
        let cfg = from_toml(
            r#"
            [build]
            profile = "dev"
            target = "wasm32-wasip1"
            target_dir = "build-out"
            locked = false
            extra_args = ["--features", "wide"]
            "#,
        )
        .unwrap();

        assert_eq!(cfg.build.profile, "dev");
        assert_eq!(cfg.build.target, "wasm32-wasip1");
        assert_eq!(cfg.build.target_dir, PathBuf::from("/proj/build-out"));
        assert!(!cfg.build.locked);
        assert_eq!(cfg.build.extra_args, vec!["--features", "wide"]);
    }

    #[test]
    fn the_api_key_can_come_from_the_file() {
        let cfg = from_toml(
            r#"
            [llm]
            api_key = "sk-from-file"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.openrouter_api_key.as_ref().unwrap().expose(), "sk-from-file");
    }

    #[test]
    fn an_absent_or_blank_key_stays_none() {
        assert!(from_toml("").unwrap().openrouter_api_key.is_none());
        let cfg = from_toml(
            r#"
            [llm]
            api_key = "   "
            "#,
        )
        .unwrap();
        assert!(
            cfg.openrouter_api_key.is_none(),
            "whitespace is not a key, and would fail confusingly at request time"
        );
    }

    #[test]
    fn the_environment_wins_over_a_configured_key() {
        let from_both = resolve_api_key(Some("sk-env".into()), "sk-file").unwrap();
        assert_eq!(from_both.expose(), "sk-env");

        let file_only = resolve_api_key(None, "sk-file").unwrap();
        assert_eq!(file_only.expose(), "sk-file");

        // Blank on either side is the same as not set at all.
        assert!(resolve_api_key(None, "").is_none());
        assert!(resolve_api_key(None, "   ").is_none());
        assert!(resolve_api_key(Some("  ".into()), "").is_none());

        // Surrounding whitespace from a copy-paste is trimmed, not sent.
        assert_eq!(
            resolve_api_key(None, "  sk-padded  ").unwrap().expose(),
            "sk-padded"
        );
    }

    #[test]
    fn a_secret_does_not_print_itself() {
        let secret = Secret::new("sk-live-do-not-log-me");
        let shown = format!("{secret:?}");
        assert!(!shown.contains("sk-live"), "{shown}");
        assert_eq!(shown, "Secret(***)");

        // The same must hold when it is nested in the struct that gets logged.
        let cfg = from_toml(
            r#"
            [llm]
            api_key = "sk-live-do-not-log-me"
            "#,
        )
        .unwrap();
        assert!(!format!("{cfg:?}").contains("sk-live"));
    }

    #[test]
    fn models_take_their_id_as_a_label_when_none_is_given() {
        let cfg = from_toml(
            r#"
            [[models]]
            id = "local/tiny"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].label, "local/tiny");
    }

    #[test]
    fn local_overlay_sits_beside_the_config_file() {
        assert_eq!(
            local_overlay_path(Path::new("/srv/app/thetis.toml")),
            PathBuf::from("/srv/app/thetis.local.toml")
        );
        assert_eq!(
            local_overlay_path(Path::new("/srv/app/staging.toml")),
            PathBuf::from("/srv/app/staging.local.toml")
        );
    }

    #[test]
    fn a_hyphenated_tool_inherits_its_group_block() {
        // The group is fictional on purpose. An earlier version of this test
        // used `notion`, and a real `NOTION_TOKEN` in the environment then
        // overrode the file it asserts on — correct behaviour, since the
        // environment beats the file, but it made the test fail wherever a
        // credential was configured. Every env-sensitive test here uses a
        // `zz*` scope no real deployment sets.
        let cfg = from_toml(
            r#"
[tools.zzgroupsvc]
token = "secret"
version = "2026-03-11"

[tools.zzgroupsvc-search]
page_size = 25
version = "2025-09-03"
"#,
        )
        .unwrap();

        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzgroupsvc-search")).unwrap();
        // Group key inherited, and the specific block wins where both name one.
        assert_eq!(seen["token"], "secret");
        assert_eq!(seen["page_size"], 25);
        assert_eq!(seen["version"], "2025-09-03");

        // A sibling with no block of its own still gets the shared credential.
        let sibling: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzgroupsvc-comment-add")).unwrap();
        assert_eq!(sibling["token"], "secret");
        assert!(sibling.get("page_size").is_none());

        // A tool outside the group sees nothing of it.
        assert_eq!(cfg.tool_config_json("web-search"), "{}");
    }

    #[test]
    fn a_file_backed_tool_secret_is_inlined_by_the_host() {
        // A tool has no filesystem import, so the host reads a `*_path` secret
        // on its behalf and hands over the contents beside it.
        let dir = std::env::temp_dir().join("thetis-secret-test");
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("app.pem");
        std::fs::write(&key_path, "-----BEGIN RSA PRIVATE KEY-----\nabc\n").unwrap();

        let file: spec::File = toml::from_str(&format!(
            "[tools.zzkeysvc]\nprivate_key_path = {:?}\n",
            key_path.to_string_lossy()
        ))
        .unwrap();
        let cfg = Config::assemble(
            dir.clone(),
            dir.join("thetis.toml"),
            file,
            Env::None,
        )
        .unwrap();

        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzkeysvc")).unwrap();
        assert!(seen["private_key_contents"]
            .as_str()
            .unwrap()
            .contains("BEGIN RSA PRIVATE KEY"));
        // The path itself is still visible, for error messages that name it.
        assert!(seen["private_key_path"].as_str().unwrap().ends_with("app.pem"));

        // A path that cannot be read reports why, rather than looking unset.
        let missing: spec::File =
            toml::from_str("[tools.zzkeysvc]\nprivate_key_path = \"nope.pem\"\n").unwrap();
        let cfg = Config::assemble(dir.clone(), dir.join("thetis.toml"), missing, Env::None).unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzkeysvc")).unwrap();
        assert!(seen.get("private_key_contents").is_none());
        let error = seen["private_key_contents_error"].as_str().unwrap();
        // A missing file must not be reported as an escape attempt: the two
        // have completely different fixes.
        assert!(
            !error.contains("inside the project root"),
            "a typo should not be blamed on confinement: {error}"
        );

        // A path outside the root is refused, and says so.
        let escaping: spec::File =
            toml::from_str("[tools.zzkeysvc]\nprivate_key_path = \"/etc/hostname\"\n").unwrap();
        let cfg = Config::assemble(dir.clone(), dir.join("thetis.toml"), escaping, Env::None).unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzkeysvc")).unwrap();
        assert!(seen.get("private_key_contents").is_none());
        assert!(seen["private_key_contents_error"]
            .as_str()
            .unwrap()
            .contains("inside the project root"));

        // A relative path resolves against the shared overlay's directory too,
        // not just the project root. This is the worktree case: a conversation
        // runs from a worktree while the credential sits beside the shared
        // overlay, so resolving only against the root would fail to find it.
        let elsewhere = std::env::temp_dir().join("thetis-secret-test-shared");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("shared.pem"), "-----BEGIN SHARED-----\n").unwrap();
        std::env::set_var("THETIS_LOCAL_CONFIG", elsewhere.join("thetis.local.toml"));

        let relative: spec::File =
            toml::from_str("[tools.zzkeysvc]\nprivate_key_path = \"shared.pem\"\n").unwrap();
        let cfg = Config::assemble(dir.clone(), dir.join("thetis.toml"), relative, Env::None).unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzkeysvc")).unwrap();
        std::env::remove_var("THETIS_LOCAL_CONFIG");
        assert!(
            seen["private_key_contents"]
                .as_str()
                .unwrap_or_default()
                .contains("BEGIN SHARED"),
            "a key beside the shared overlay should be found: {seen}"
        );

        std::fs::remove_dir_all(&elsewhere).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tool_credential_can_come_from_the_environment() {
        // Serialised against the other env-touching tests via a shared lock
        // would be safer still; here the variable names are unique to this
        // test, so no other test can observe them.
        std::env::set_var("ZZTESTSVC_API_KEY", "from-env");
        std::env::set_var("THETIS_TOOL_ZZTESTSVC_PAGE_SIZE", "40");

        let cfg = from_toml("[tools.zztestsvc]\nversion = \"1\"\n").unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zztestsvc-search")).unwrap();

        // The file's own key survives; the environment supplies the rest.
        assert_eq!(seen["version"], "1");
        assert_eq!(seen["token"], "from-env");
        assert_eq!(seen["page_size"], "40");

        std::env::remove_var("ZZTESTSVC_API_KEY");
        std::env::remove_var("THETIS_TOOL_ZZTESTSVC_PAGE_SIZE");
    }

    #[test]
    fn the_environment_beats_the_file_for_the_same_key() {
        std::env::set_var("ZZOTHERSVC_TOKEN", "env-wins");

        let cfg = from_toml("[tools.zzothersvc]\ntoken = \"file-loses\"\n").unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzothersvc-get")).unwrap();
        assert_eq!(seen["token"], "env-wins");

        std::env::remove_var("ZZOTHERSVC_TOKEN");
    }

    #[test]
    fn an_unset_environment_leaves_a_tool_with_just_its_file_block() {
        let cfg = from_toml("[tools.zzquietsvc]\nversion = \"1\"\n").unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(&cfg.tool_config_json("zzquietsvc-get")).unwrap();
        assert!(seen.get("token").is_none(), "invented a token: {seen}");
    }

    #[test]
    fn overlay_sets_one_key_without_restating_its_section() {
        let mut base: toml::Value = toml::from_str(
            "[llm]
model = \"a/b\"
max_retries = 3

[tools.web-search]
timeout = 30
",
        )
        .unwrap();
        let overlay: toml::Value =
            toml::from_str("[tools.web-search]
api_key = \"secret\"
").unwrap();

        merge_toml(&mut base, overlay);

        // The overlay added its key and left the neighbours alone.
        let tools = base.get("tools").unwrap().get("web-search").unwrap();
        assert_eq!(tools.get("api_key").unwrap().as_str(), Some("secret"));
        assert_eq!(tools.get("timeout").unwrap().as_integer(), Some(30));
        assert_eq!(
            base.get("llm").unwrap().get("model").unwrap().as_str(),
            Some("a/b")
        );
    }

    #[test]
    fn overlay_replaces_scalars_and_arrays_rather_than_merging_them() {
        let mut base: toml::Value =
            toml::from_str("[build]
locked = true
allowed_crates = [\"a\", \"b\"]
").unwrap();
        let overlay: toml::Value =
            toml::from_str("[build]
locked = false
allowed_crates = [\"c\"]
").unwrap();

        merge_toml(&mut base, overlay);

        let build = base.get("build").unwrap();
        assert_eq!(build.get("locked").unwrap().as_bool(), Some(false));
        // Replaced, not appended: otherwise a list could never be shortened.
        let crates = build.get("allowed_crates").unwrap().as_array().unwrap();
        assert_eq!(crates.len(), 1);
        assert_eq!(crates[0].as_str(), Some("c"));
    }

    /// The config that ships with the repo must actually load.
    ///
    /// Worth guarding even now that an unknown key only warns: a key with
    /// thetis.toml without adding it to the spec is an error that only shows
    /// up at startup, on whichever machine picks the file up first.
    #[test]
    fn the_shipped_config_file_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let path = root.join("thetis.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

        Config::validate(&text, root)
            .unwrap_or_else(|e| panic!("the shipped thetis.toml does not load: {e:#}"));
    }

    #[test]
    fn a_mode_can_carry_a_prompt() {
        let cfg = from_toml(
            r#"
[[modes]]
id = "agent"
label = "Agent"

[[modes]]
id = "plan"
read_only = true
prompt = "Plan only."
"#,
        )
        .unwrap();

        let plan = cfg.modes.iter().find(|m| m.id == "plan").unwrap();
        assert!(plan.read_only);
        assert_eq!(plan.prompt, "Plan only.");

        // A mode that says nothing extra carries an empty prompt, not a default.
        let agent = cfg.modes.iter().find(|m| m.id == "agent").unwrap();
        assert!(agent.prompt.is_empty());
    }

    #[test]
    fn the_chat_mode_shipped_for_messaging_surfaces_is_read_only() {
        // The Discord connector relies on this: it stamps every session it
        // creates with "chat", and that is the whole of its tool restriction.
        let cfg = from_toml(&std::fs::read_to_string("../../thetis.toml").unwrap()).unwrap();
        let chat = cfg.mode("chat").expect("chat mode should be configured");
        assert!(chat.read_only, "chat mode must stay read-only");
    }
}
