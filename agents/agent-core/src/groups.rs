//! Task-dependent scoping of the tool surface.
//!
//! Every tool belongs to exactly one group. A group is a *task* allocation, not
//! a capability one: `available` in `tools.rs` already answers "is this
//! possible here", and the answer to that is a flat list of everything the
//! machine can do. This answers a different question — "is this plausibly
//! wanted in *this* conversation" — and a session doing code review has no use
//! for BigQuery, Notion or the ssh registry.
//!
//! # Why
//!
//! A large flat tool block costs tokens and, less obviously, accuracy. The
//! published numbers are consistent about both directions:
//!
//! * LongFuncEval (arXiv 2505.10570) measured 7–85% drops in tool-calling
//!   accuracy as the catalog grew from 8K to 120K tokens.
//! * MCPGauge (2508.12566) found naive attachment of MCP tool definitions
//!   *reduced* accuracy by 9.5% on average across six commercial models, while
//!   multiplying input tokens 3.25×–236.5×.
//! * Anthropic's tool-search work reports Opus 4 rising 49%→74% and Opus 4.5
//!   79.5%→88.1% on a 58-tool surface when definitions are fetched on demand
//!   rather than preloaded, at ~85% fewer tokens.
//! * Set against that, "How Many Tools Should an LLM Agent See?" (2605.24660)
//!   found a fixed cut-off beating an adaptive one in aggregate on a 3,251-tool
//!   corpus while scoring 0% on hard queries where the adaptive one got 16.7%.
//!   A candidate that never enters the pool cannot be recovered downstream.
//!
//! So the design is deliberately asymmetric. Withholding a group is cheap to
//! get wrong in tokens and expensive to get wrong in capability, therefore:
//!
//! * routing is generous — one tag match admits a group;
//! * the groups that almost every task needs are always on;
//! * `tool_search` can pull in any group mid-session, append-only;
//! * and the whole thing is off by default until the accounting says it pays.
//!
//! # Evidence used for routing
//!
//! Three signals, unioned rather than ranked, because they fail in different
//! places:
//!
//! 1. **Always-on.** Declared in the table below, plus anything named in
//!    `tool_groups.always_on` config.
//! 2. **Skill edges.** A retrieved skill carrying a `tool-group:<id>` tag
//!    admits that group. This is the strongest signal available, and it is
//!    nearly free: `when_to_use` is already written in the user's language and
//!    already embedded and benchmarked, which makes a skill card a better
//!    retrieval surrogate for a tool group than the tool descriptions are for
//!    themselves — the finding behind ToolRet (2503.01763), whose best model
//!    manages only nDCG@10 = 33.83 against raw tool docs, and Tool-DE
//!    (2510.22670), which recovers ≈+10 nDCG@10 purely by expanding those docs
//!    with the same fields a SKILL.md already has.
//! 3. **Lexical tag match** on the opening message. Lexical, not dense, on
//!    purpose: the in-repo benchmark in `skill_index.rs` found dense beat every
//!    RRF mixing weight for *fine-ranking* paraphrased skill cards, but routing
//!    among a dozen groups with near-disjoint vocabularies is the case where
//!    cheap lexical signal does well (ToolScout, 2608.16502, found query-side
//!    TF-IDF distance a better detector of distribution shift than BGE-M3,
//!    r=−0.85 vs −0.58). It also costs no embedding call and cannot fail.
//!
//! # Prompt cache
//!
//! The active set is pinned in session KV on the first turn and read back
//! afterwards, for the same reason skill retrieval is pinned: the request must
//! be byte-identical between turns or the provider's prompt cache misses.
//! `tool_search` deliberately breaks that once, when it fires, and never
//! removes a group — so the set only ever grows and the cache re-warms.

use crate::thetis::grip::types::LogLevel;
use crate::thetis::grip::{skills, sys, tooling};

/// Session KV key holding the pinned active group ids, newline separated.
///
/// Mirrored as a string literal in the web gateway, which reads this to show
/// what is attached and writes it to override the routing. The two cannot share
/// a constant — they are separate guest components — so a rename here means a
/// grep for the literal there. `publish_table` exists so that the *table* at
/// least is never duplicated that way.
pub const PIN_KEY: &str = "__tool_groups";

/// Global KV key holding the group table as JSON.
///
/// Published by the agent, read by the chat surface. Global rather than
/// per-session because the table is a property of the build, not of one
/// conversation, and static enough that republishing it is idempotent.
///
/// This exists so the panel does not need a live worker. `available-tools` asks
/// the agent directly and so cannot drift, but it routes through
/// `workers::call_session`, and a worker is the shortest-lived thing in the
/// system — reaped when idle, restarted onto kernels it built. A panel that
/// spawned one as a side effect of being opened would also be empty for exactly
/// the conversations someone is most likely to be inspecting: stopped and
/// archived ones. So the durable store is the contract instead.
pub const TABLE_KEY: &str = "__tool_group_table";

/// Session KV key holding `id=reason` lines: why each active group is active.
///
/// Kept beside the pin rather than inside it because the pin is on the hot path
/// — read once per turn to build the tool block — and provenance is only ever
/// read by the panel. Reasons are advisory: a missing or stale line costs an
/// explanation in the UI, never a capability.
pub const WHY_KEY: &str = "__tool_groups_why";

/// Why a group is in the active set. The panel shows this so an attached group
/// is never unexplained, and so a user overriding the routing can see what they
/// are overriding.
///
/// A shared vocabulary rather than an enum, because the writers live in two
/// components: the reasons below are written here during routing, while
/// `manual` is written by the web gateway when a user overrides. Nothing
/// validates them — an unrecognised reason renders as itself.
#[allow(dead_code)] // written by the web gateway, read here and by the panel.
pub const REASON_MANUAL: &str = "manual";
pub const REASON_ALWAYS_ON: &str = "always-on";
pub const REASON_CONFIGURED: &str = "configured";
pub const REASON_SKILL: &str = "skill";
pub const REASON_TAG: &str = "tag";
pub const REASON_SEARCH: &str = "search";

/// The tag prefix a skill uses to point at a tool group.
const SKILL_TAG_PREFIX: &str = "tool-group:";

/// The capability prefix a hot-loaded tool component uses to declare which
/// group it belongs to. Uses the existing `capabilities` list, so this needs no
/// change to the WIT contract.
const COMPONENT_CAP_PREFIX: &str = "group:";

/// Group id used for a component that declares nothing and matches no prefix
/// rule. Always on: an unclassified tool must not vanish because nobody
/// remembered to tag it.
const UNGROUPED: &str = "extra";

pub struct ToolGroup {
    pub id: &'static str,
    /// One line, for `tool_search` results and the system prompt.
    pub brief: &'static str,
    /// Words that mean this group is wanted, matched against the opening
    /// message. Single lowercase tokens; stemming is not attempted.
    pub tags: &'static [&'static str],
    /// Admitted for every session without evidence.
    pub always_on: bool,
    /// Built-in tool names in this group. Hot-loaded components are matched by
    /// capability or prefix instead, in `component_group`.
    pub members: &'static [&'static str],
}

/// The group table.
///
/// Membership is a task cut, so it deliberately crosses the capability-shaped
/// `*_tools()` functions in `tools.rs`: `git_clone` sits with the shell rather
/// than with the GitHub API tools, and `restart_orchestrator` sits with
/// self-modification rather than with configuration.
pub fn all() -> &'static [ToolGroup] {
    &[
        ToolGroup {
            id: "core",
            brief: "Memory and asking the user questions.",
            tags: &[],
            always_on: true,
            // `tool_search` lives here because it is the route by which every
            // withheld group is recovered. Putting it in a scopable group would
            // make the escape hatch itself losable.
            members: &["remember", "recall", "ask_user", "tool_search"],
        },
        ToolGroup {
            id: "skills",
            brief: "Reading, searching and authoring skills.",
            tags: &["skill", "skills"],
            // The disclosure mechanism itself. Withholding it would hide the
            // route by which everything else is discovered.
            always_on: true,
            members: &[
                "skill_fetch",
                "skill_search",
                "skill_write",
                "skill_delete",
                "skill_lint",
            ],
        },
        ToolGroup {
            id: "files",
            brief: "Reading, searching, writing and deleting files on the host.",
            tags: &[
                "file", "files", "read", "write", "edit", "search", "grep", "directory", "path",
                "code", "source",
            ],
            // Almost every task touches a file, and these are the tools the
            // system prompt tells the model to prefer over shell equivalents.
            always_on: true,
            members: &[
                "read_path",
                "edit_path",
                "search_files",
                "find_files",
                "write_path",
                "list_path",
                "delete_path",
            ],
        },
        ToolGroup {
            id: "shell",
            brief: "Terminal sessions for builds, tests, git and long-running processes.",
            tags: &[
                "run", "build", "test", "tests", "compile", "command", "shell", "terminal",
                "bash", "process", "script", "cargo", "npm", "python", "git", "clone", "install",
                "deploy",
            ],
            always_on: false,
            members: &[
                "terminal_open",
                "terminal_run",
                "terminal_read",
                "terminal_send",
                "terminal_signal",
                "terminal_close",
                "terminal_list",
                "git_clone",
            ],
        },
        ToolGroup {
            id: "ssh",
            brief: "The named ssh host registry, for shell sessions on other machines.",
            tags: &["ssh", "remote", "host", "hosts", "machine", "server", "box"],
            always_on: false,
            members: &[
                "ssh_host_list",
                "ssh_host_get",
                "ssh_host_set",
                "ssh_host_remove",
                "ssh_host_rename",
            ],
        },
        ToolGroup {
            id: "selfmod",
            brief: "The dev kit: editing and rebuilding your own loop, gateways and tools.",
            // Deliberately *not* tagged "tool", "tools", "agent" or "self".
            // Those words appear in any conversation that merely discusses
            // tooling — including this group's own reason for existing — so they
            // admitted the dev kit almost unconditionally and cost the tokens
            // that scoping is meant to save. The distinctive words are the ones
            // that only occur when the work really is self-modification.
            //
            // The loss is covered: `thetis-internals/tool-authorship` and
            // `careful-surgery` both carry `tool-group:selfmod`, so retrieving
            // either admits this group, and a stray call admits it at invoke
            // time. Skill edges are a better signal here than keywords.
            tags: &[
                "yourself", "loop", "gateway", "devkit", "component", "wasm", "rebuild",
                "recompile", "dependency", "crate", "restart", "scaffold",
            ],
            always_on: false,
            members: &[
                "new_tool",
                "write_code",
                "patch_code",
                "add_dependency",
                "remove_dependency",
                "list_dependencies",
                "read_code",
                "list_code",
                "restart_orchestrator",
            ],
        },
        ToolGroup {
            id: "branch",
            brief: "This conversation's sandbox branch: status, history, trunk merges, rollback.",
            // "sandbox" belonged to the sandbox group and pulled this one in
            // with it; "history" is generic enough to match any conversation
            // about the past.
            tags: &[
                "branch", "trunk", "merge", "commit", "rollback", "revert", "reset", "conflict",
            ],
            always_on: false,
            members: &[
                "branch_status",
                "branch_log",
                "update_from_trunk",
                "reset_branch",
                "complete_merge",
                "abort_merge",
            ],
        },
        ToolGroup {
            id: "config",
            brief: "Reading and changing Thetis's own settings.",
            tags: &["config", "configuration", "setting", "settings", "toml", "option"],
            always_on: false,
            members: &["list_config", "read_config", "set_config"],
        },
        ToolGroup {
            id: "sandbox",
            brief: "The isolated sandbox: running a command, reading and writing its files.",
            // Kept narrow on purpose: these three tools shadow the host
            // filesystem and terminal tools by name-alike, and admitting them
            // alongside is how a model ends up writing to the wrong place.
            tags: &["sandbox", "isolated", "scratch"],
            always_on: false,
            members: &["exec", "write_file", "read_file"],
        },
        // --- groups whose members are hot-loaded components -----------------
        ToolGroup {
            id: "bigquery",
            brief: "BigQuery: listing, describing, profiling, querying and costing tables.",
            // "query", "table", "rows" and "schema" are all common in
            // conversations with no database in them — "query" especially, since
            // it is the word for what a retriever takes. The remaining tokens
            // are ones that really do imply a warehouse.
            tags: &[
                "bigquery", "bq", "sql", "dataset", "warehouse", "gcp", "partition",
                "analytics",
            ],
            always_on: false,
            members: &[],
        },
        ToolGroup {
            id: "notion",
            brief: "Notion: pages, databases, comments and users in a workspace.",
            // "page", "workspace", "database" and "doc" all mean something else
            // in a coding conversation — a workspace is a Cargo concept here.
            tags: &["notion", "wiki"],
            always_on: false,
            members: &[],
        },
        ToolGroup {
            id: "web",
            brief: "Web search, page fetching and cited summarisation.",
            // "search" is dropped: it is already a `files` tag and means the
            // local tree far more often than the internet. "documentation" goes
            // the same way — usually it is the repo's own docs.
            tags: &[
                "web", "internet", "online", "arxiv", "paper", "papers", "research", "url",
                "link", "article", "news", "blog", "google",
            ],
            always_on: false,
            members: &[],
        },
        ToolGroup {
            id: "github",
            brief: "The GitHub API: reading and committing files, repos, branches, PRs.",
            // "commit" is a `branch` tag and means the local branch more often;
            // "pull" and "push" are ambiguous with plain git over the shell.
            tags: &["github", "repo", "repository", "pr", "issue", "upstream", "clone"],
            always_on: false,
            members: &[],
        },
        ToolGroup {
            id: UNGROUPED,
            brief: "Tools that declare no group.",
            tags: &[],
            // An unclassified component keeps working. Tagging is an
            // optimisation, never a precondition for being callable.
            always_on: true,
            members: &[],
        },
    ]
}

fn find(id: &str) -> Option<&'static ToolGroup> {
    all().iter().find(|g| g.id == id)
}

/// Every group id. Used where "no scoping" has to be expressed as a set.
pub fn all_ids() -> Vec<String> {
    all().iter().map(|g| g.id.to_string()).collect()
}

/// Name-prefix rules for hot-loaded components that have not declared a group.
///
/// A stopgap by design: a component *should* say `group:<id>` in its
/// capabilities. Until it does, the naming convention already in use carries
/// the same information and costs nothing to read.
const PREFIX_RULES: &[(&str, &str)] = &[
    ("bq-", "bigquery"),
    ("notion-", "notion"),
    ("web-", "web"),
    ("git-", "github"),
];

/// Which group a hot-loaded component belongs to: its own declaration first,
/// then the naming convention, then `extra`.
///
/// Public because the callers that already hold a manifest should use this
/// rather than `group_of`, which would fetch the registry again for information
/// it is standing on. That mattered: `definitions_for` runs once per turn over
/// every component, so the naive version was a host call per tool per tool.
pub fn component_group(name: &str, capabilities: &[String]) -> String {
    for cap in capabilities {
        if let Some(id) = cap.strip_prefix(COMPONENT_CAP_PREFIX) {
            let id = id.trim();
            if find(id).is_some() {
                return id.to_string();
            }
            // A component naming a group that does not exist is a bug in the
            // component, not grounds for hiding it.
            sys::log(
                LogLevel::Warn,
                &format!("tool '{name}' declares unknown group '{id}'; treating as {UNGROUPED}"),
            );
            return UNGROUPED.to_string();
        }
    }
    for (prefix, id) in PREFIX_RULES {
        if name.starts_with(prefix) {
            return (*id).to_string();
        }
    }
    UNGROUPED.to_string()
}

/// Which group a tool name belongs to, built-in or component.
pub fn group_of(name: &str) -> String {
    if let Some(group) = all().iter().find(|g| g.members.contains(&name)) {
        return group.id.to_string();
    }
    match tooling::registry().into_iter().find(|m| m.name == name) {
        Some(manifest) => component_group(&manifest.name, &manifest.capabilities),
        // Not a built-in and not registered: nothing to withhold it from.
        None => UNGROUPED.to_string(),
    }
}

/// Whether a tool is in the active set.
///
/// Costs a registry fetch for a name that is not a built-in; prefer
/// [`builtin_active`] plus [`component_group`] where the manifest is already to
/// hand.
pub fn is_active(name: &str, active: &[String]) -> bool {
    let group = group_of(name);
    active.iter().any(|a| *a == group)
}

/// Whether a built-in is in the active set, without touching the registry.
///
/// A built-in that appears in no group falls back to `extra`, which is always
/// on — so a forgotten table entry costs tokens, never a capability.
pub fn builtin_active(name: &str, active: &[String]) -> bool {
    let group = all()
        .iter()
        .find(|g| g.members.contains(&name))
        .map(|g| g.id)
        .unwrap_or(UNGROUPED);
    active.iter().any(|a| a == group)
}

/// Built-ins that belong to no group, and groups naming a tool that does not
/// exist.
///
/// The failure this catches is asymmetric and quiet: add a built-in, forget the
/// group table, and with grouping off nothing happens at all — the tool works,
/// tests pass, and the omission surfaces months later as a tool that is simply
/// never offered to anyone. So it is checked at startup and reported, and the
/// fallback for an unlisted built-in is `extra`, which is always on.
pub fn coverage_gaps(builtins: &[String]) -> (Vec<String>, Vec<String>) {
    let ungrouped: Vec<String> = builtins
        .iter()
        .filter(|name| !all().iter().any(|g| g.members.contains(&name.as_str())))
        .cloned()
        .collect();

    let phantom: Vec<String> = all()
        .iter()
        .flat_map(|g| g.members.iter())
        .filter(|m| !builtins.iter().any(|b| b == *m))
        .map(|m| m.to_string())
        .collect();

    (ungrouped, phantom)
}

/// Logs any coverage gap. Called once per turn from the loop; cheap enough, and
/// a warning nobody sees is worth less than one repeated.
pub fn check_coverage(builtins: &[String]) {
    let (ungrouped, phantom) = coverage_gaps(builtins);
    if !ungrouped.is_empty() {
        sys::log(
            LogLevel::Warn,
            &format!(
                "tool groups: {} built-in(s) belong to no group and fall back to '{UNGROUPED}': {}",
                ungrouped.len(),
                ungrouped.join(", ")
            ),
        );
    }
    if !phantom.is_empty() {
        sys::log(
            LogLevel::Warn,
            &format!(
                "tool groups: group table names {} tool(s) that do not exist: {}",
                phantom.len(),
                phantom.join(", ")
            ),
        );
    }
}

// --- routing ----------------------------------------------------------------

/// Tokenises text into lowercase word-ish runs for tag matching.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// How strongly a query calls for a group, in 0.0..1.0.
///
/// Saturating rather than proportional: `m / (m + 1)` means one distinct tag
/// match already scores 0.5, and further matches add less. Proportional scoring
/// (`matched / total`) would punish a group for being described thoroughly,
/// which is exactly backwards — a group with more tags is easier to route to,
/// not harder.
pub fn score(group: &ToolGroup, query_tokens: &[String]) -> f64 {
    if group.tags.is_empty() {
        return 0.0;
    }
    let matches = group
        .tags
        .iter()
        .filter(|tag| query_tokens.iter().any(|t| t == *tag))
        .count() as f64;
    matches / (matches + 1.0)
}

/// Group ids a set of skill cards points at, via their `tool-group:` tags.
fn groups_from_skills(cards: &[skills::SkillCard]) -> Vec<String> {
    let mut out = Vec::new();
    for card in cards {
        for tag in &card.tags {
            if let Some(id) = tag.strip_prefix(SKILL_TAG_PREFIX) {
                let id = id.trim();
                if find(id).is_some() {
                    if !out.iter().any(|g| g == id) {
                        out.push(id.to_string());
                    }
                } else {
                    sys::log(
                        LogLevel::Warn,
                        &format!("skill '{}' points at unknown tool group '{id}'", card.id),
                    );
                }
            }
        }
    }
    out
}

/// Decides the active set for a conversation and pins it. Idempotent: once
/// pinned, the pin is what is returned, so the tool block stays byte-identical
/// between turns.
///
/// Returns the ids in table order, which keeps the serialised tool list stable
/// regardless of the order evidence arrived in.
pub fn route_once(session_id: &str, query: &str) -> Vec<String> {
    if let Some(pinned) = read_pin(session_id) {
        return pinned;
    }

    // Every admission records why, so the panel can explain an attached group
    // rather than presenting it as a bare fact.
    let mut why: Vec<(String, &str)> = Vec::new();

    let mut active: Vec<String> = all()
        .iter()
        .filter(|g| g.always_on)
        .map(|g| g.id.to_string())
        .collect();
    for id in &active {
        why.push((id.clone(), REASON_ALWAYS_ON));
    }

    // Configured always-on, for a deployment that wants a group unconditionally
    // without editing this table.
    for id in configured_always_on() {
        if find(&id).is_some() && !active.contains(&id) {
            why.push((id.clone(), REASON_CONFIGURED));
            active.push(id);
        }
    }

    // Skill edges. The strongest signal, so it is applied before the lexical
    // pass and unconditionally.
    let from_skills = groups_from_skills(&skills::pinned(session_id));
    for id in &from_skills {
        if !active.contains(id) {
            why.push((id.clone(), REASON_SKILL));
            active.push(id.clone());
        }
    }

    // Lexical tag match on the opening message.
    let threshold = route_threshold();
    let query_tokens = tokens(query);
    let mut from_tags = Vec::new();
    for group in all() {
        if active.iter().any(|a| a == group.id) {
            continue;
        }
        let s = score(group, &query_tokens);
        if s >= threshold {
            active.push(group.id.to_string());
            why.push((group.id.to_string(), REASON_TAG));
            from_tags.push(format!("{} {:.2}", group.id, s));
        }
    }

    let ordered = in_table_order(&active);
    write_pin(session_id, &ordered);
    note_reasons(session_id, &why);

    sys::log(
        LogLevel::Debug,
        &format!(
            "tool groups routed: active=[{}] from-skills=[{}] from-tags=[{}]",
            ordered.join(", "),
            from_skills.join(", "),
            from_tags.join(", ")
        ),
    );

    ordered
}

/// Adds groups to a session's active set, for `tool_search`. Append-only:
/// nothing already admitted is ever taken away, so a tool the model has already
/// seen cannot disappear mid-conversation.
pub fn admit(session_id: &str, ids: &[String]) -> Vec<String> {
    let mut active = read_pin(session_id).unwrap_or_else(|| {
        all()
            .iter()
            .filter(|g| g.always_on)
            .map(|g| g.id.to_string())
            .collect()
    });
    let mut added = Vec::new();
    for id in ids {
        if find(id).is_some() && !active.contains(id) {
            active.push(id.clone());
            added.push(id.clone());
        }
    }
    if !added.is_empty() {
        write_pin(session_id, &in_table_order(&active));
        let pairs: Vec<(String, &str)> =
            added.iter().map(|id| (id.clone(), REASON_SEARCH)).collect();
        note_reasons(session_id, &pairs);
    }
    added
}

/// The active set for a session, without routing. Used per turn, where the
/// answer must come from the pin rather than be recomputed.
pub fn active(session_id: &str) -> Vec<String> {
    read_pin(session_id).unwrap_or_else(|| all().iter().map(|g| g.id.to_string()).collect())
}

fn in_table_order(ids: &[String]) -> Vec<String> {
    all()
        .iter()
        .filter(|g| ids.iter().any(|i| i == g.id))
        .map(|g| g.id.to_string())
        .collect()
}

/// Reads the pinned set, and repairs it.
///
/// The pin is a shared contract, not private state: the chat surface writes it
/// directly to override the routing, because the gateway is a separate
/// component and cannot call into here. So it is treated as untrusted input on
/// every read — unknown ids dropped, always-on groups forced back in, order
/// normalised.
///
/// Forcing always-on back in is the safety property that makes a manual
/// override safe to offer at all. `core` holds `tool_search`, the route by
/// which every withheld group is recovered; a UI bug, a hand-edited store or a
/// future writer that dropped it would leave a conversation unable to get any
/// tool back. Repairing on read means the invariant holds no matter who wrote.
fn read_pin(session_id: &str) -> Option<Vec<String>> {
    repair_pin(&sys::kv_get(session_id, PIN_KEY)?)
}

/// The repair itself, split from the host call so it can be tested.
///
/// See `read_pin` for why this exists. Returning `None` for an empty or
/// unrecognisable pin is the important half: it means "never routed", so the
/// caller falls open to every group. Treating it as "route to nothing" would
/// turn a corrupt value into a conversation with no tools at all.
fn repair_pin(raw: &str) -> Option<Vec<String>> {
    let mut ids: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| find(l).is_some())
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        return None;
    }
    for group in all().iter().filter(|g| g.always_on) {
        if !ids.iter().any(|i| i == group.id) {
            ids.push(group.id.to_string());
        }
    }
    Some(in_table_order(&ids))
}

fn write_pin(session_id: &str, ids: &[String]) {
    sys::kv_put(session_id, PIN_KEY, &ids.join("\n"));
}

/// Records why each active group is active, for the panel.
///
/// Merges rather than replaces: `admit` adds one group at a time and must not
/// erase the reasons already established by routing.
fn note_reasons(session_id: &str, pairs: &[(String, &str)]) {
    let mut existing = read_reasons(session_id);
    for (id, reason) in pairs {
        if let Some(slot) = existing.iter_mut().find(|(e, _)| e == id) {
            slot.1 = reason.to_string();
        } else {
            existing.push((id.clone(), reason.to_string()));
        }
    }
    let body = existing
        .iter()
        .map(|(id, reason)| format!("{id}={reason}"))
        .collect::<Vec<_>>()
        .join("\n");
    sys::kv_put(session_id, WHY_KEY, &body);
}

fn read_reasons(session_id: &str) -> Vec<(String, String)> {
    sys::kv_get(session_id, WHY_KEY)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (id, reason) = line.split_once('=')?;
            let id = id.trim();
            if id.is_empty() {
                return None;
            }
            Some((id.to_string(), reason.trim().to_string()))
        })
        .collect()
}

/// Publishes the group table so the chat surface can render it without a live
/// worker. Cheap, idempotent, and called once per turn whether or not grouping
/// is enabled — the panel has to be able to say "scoping is off" too.
pub fn publish_table() {
    let groups: Vec<String> = all()
        .iter()
        .map(|g| {
            format!(
                r#"{{"id":{},"brief":{},"tags":{},"always_on":{},"members":{}}}"#,
                json_str(g.id),
                json_str(g.brief),
                json_array(g.tags),
                g.always_on,
                json_array(g.members),
            )
        })
        .collect();
    let payload = format!(
        r#"{{"enabled":{},"threshold":{},"ungrouped":{},"groups":[{}]}}"#,
        grouping_enabled(),
        route_threshold(),
        json_str(UNGROUPED),
        groups.join(","),
    );
    sys::kv_put("global", TABLE_KEY, &payload);
}

/// Minimal JSON string escaping. The agent has no serialiser linked, and the
/// only values passing through are compile-time table entries, but a stray
/// quote would corrupt the whole payload silently — so it is escaped properly
/// rather than trusted.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_array(items: &[&str]) -> String {
    format!(
        "[{}]",
        items.iter().map(|i| json_str(i)).collect::<Vec<_>>().join(",")
    )
}

// --- configuration ----------------------------------------------------------

pub fn grouping_enabled() -> bool {
    sys::config_get("tool_grouping_enabled").as_deref() == Some("true")
}

pub fn accounting_enabled() -> bool {
    // Absent means on: the measurement is the point, and a kernel that predates
    // the key should still produce the baseline.
    sys::config_get("tool_accounting_enabled").as_deref() != Some("false")
}

fn route_threshold() -> f64 {
    sys::config_get("tool_route_threshold")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.15)
}

fn configured_always_on() -> Vec<String> {
    sys::config_get("tool_groups_always_on")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// --- description ------------------------------------------------------------

/// The catalogue `tool_search` ranks against, and the summary the system prompt
/// shows. Groups already active are marked, so the model can tell what it
/// already has from what it could ask for.
pub fn catalogue(active: &[String]) -> String {
    let mut out = String::new();
    for group in all() {
        if group.id == UNGROUPED {
            continue;
        }
        let mark = if active.iter().any(|a| a == group.id) {
            "loaded"
        } else {
            "available"
        };
        out.push_str(&format!("- `{}` [{}] — {}\n", group.id, mark, group.brief));
    }
    out
}

/// Ranks groups against a query for `tool_search`, best first, with the
/// always-generous threshold ignored: an explicit search is an explicit
/// request, so a weak match is still worth showing.
pub fn rank(query: &str) -> Vec<(&'static ToolGroup, f64)> {
    let query_tokens = tokens(query);
    let mut scored: Vec<(&'static ToolGroup, f64)> = all()
        .iter()
        .filter(|g| g.id != UNGROUPED)
        .map(|g| (g, score(g, &query_tokens)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(b.0.id)));
    scored
}
