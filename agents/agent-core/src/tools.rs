//! The agent's tool surface.
//!
//! Tools are advertised only when the capability behind them is actually
//! available, so the model is never offered something that will just fail. As
//! the orchestrator gains capabilities (the sandbox, the dev kit), the flags
//! flip and the tools appear without the agent needing to change.

use crate::thetis::grip::types::{
    CompileReport, ConfigEntry, Dependency, EnvVar, EventRecord, ExecResult, FsEntry, LogLevel,
    ModTarget, SessionEvent, SshHostInfo, TerminalOpen, TerminalOutput, ToolManifest,
};
use crate::thetis::grip::{
    branch, configuration, control, delegation, devkit, hostfs, sandbox, skills, sys, terminal,
    tooling, transcripts,
};
use crate::groups;
use crate::plan;
use crate::todos;
use serde_json::{json, Value};

/// The mode assumed when a session has not chosen one.
pub const DEFAULT_MODE: &str = "agent";

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// Whether calling this changes something outside the conversation.
    /// Read-only modes withhold the ones that do.
    pub mutating: bool,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn string_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn sandbox_available() -> bool {
    sandbox::available()
}

fn devkit_available() -> bool {
    sys::config_get("devkit_available").as_deref() == Some("true")
}

fn filesystem_available() -> bool {
    hostfs::available()
}

fn terminal_available() -> bool {
    terminal::available()
}

fn restart_available() -> bool {
    control::available()
}

/// Whether this session may spawn sub-agents.
///
/// False in two cases the agent cannot tell apart, and should not need to:
/// delegation is switched off in configuration, or this session *is* a
/// sub-agent. The second is the one-level rule, and the host owns it — asking
/// here keeps the guest from having to know its own parentage.
fn delegation_available() -> bool {
    delegation::available()
}

/// Whether a mode withholds tools that change things.
///
/// Asked of the grip rather than hardcoded, so adding a read-only mode is a
/// configuration change and needs nothing here.
fn read_only(mode: &str) -> bool {
    sys::list_modes()
        .into_iter()
        .find(|m| m.id == mode)
        .map(|m| m.read_only)
        .unwrap_or(false)
}

/// Whether a tool name changes something outside the conversation.
///
/// Built-ins are checked against their own declarations; anything else is a
/// hot-loaded tool component, whose behaviour is opaque and so treated as
/// mutating.
fn is_mutating(name: &str) -> bool {
    match all_builtins().into_iter().find(|t| t.name == name) {
        Some(tool) => tool.mutating,
        // Not a built-in, so it is a hot-loaded component. It may declare
        // itself read-safe; absent that declaration it is treated as mutating,
        // so the default stays closed for a tool that says nothing.
        None => !component_read_safe(name),
    }
}

/// The capability string a tool component uses to declare that it changes
/// nothing outside the conversation.
///
/// A tool asserts this about itself, so it is only as trustworthy as the tool.
/// It is meant for genuinely read-only work — a web fetch, a lookup — and it is
/// what lets such a tool survive a read-only mode instead of being withheld
/// along with everything else opaque.
const READ_ONLY_CAP: &str = "read-only";

/// Whether a hot-loaded component declares itself read-safe.
///
/// Unknown names answer `false`: a tool that is not in the registry cannot have
/// declared anything, and guessing in its favour would be the wrong default.
fn component_read_safe(name: &str) -> bool {
    tooling::registry()
        .into_iter()
        .find(|m| m.name == name)
        .map(|m| m.capabilities.iter().any(|c| c == READ_ONLY_CAP))
        .unwrap_or(false)
}

/// Every built-in the agent knows about, whether or not it is currently
/// offered.
///
/// `available` already includes the dev kit when it is enabled; this adds it
/// back when it is not, so a name can still be classified as mutating even in
/// a configuration where it is never offered.
fn all_builtins() -> Vec<ToolDef> {
    let mut tools = available(DEFAULT_MODE);
    if !sandbox_available() {
        tools.extend(sandbox_tools());
    }
    if !devkit_available() {
        tools.extend(devkit_tools());
    }
    if !filesystem_available() {
        tools.extend(filesystem_tools());
    }
    if !terminal_available() {
        tools.extend(terminal_tools());
        tools.extend(git_tools());
    }
    // Classification must cover them either way: whether the ssh_host_* tools
    // are offered depends on config, but each one's mutating flag does not.
    if !(terminal_available() && terminal::ssh_available()) {
        tools.extend(ssh_host_tools());
    }
    // A sub-agent is not offered these, but it can still name one — and the
    // classification decides whether a read-only mode refuses it, so it has to
    // be right even where the tool is withheld.
    if !delegation_available() {
        tools.extend(subagent_tools());
    }
    if !restart_available() {
        tools.extend(restart_tools());
    }
    tools
}

/// Restarting the runtime, which is how a change to the kernel or to a
/// startup-only setting takes effect.
///
/// One tool, but still a named group rather than an inline block in
/// `available`, for the reason given on every other group here: a tool defined
/// inline cannot be added back by `all_builtins`, so in a deployment where the
/// capability is off the name becomes unclassifiable — `is_mutating` falls
/// through to the component path and guesses, and the group table reports the
/// `selfmod` entry for it as a phantom.
fn restart_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "restart_orchestrator",
        description: "Restart this conversation's own runtime — no other conversation notices. \
             Needed for changes to settings read only at startup, and for changes to \
             the orchestrator's own source under crates/. Do NOT build the \
             orchestrator yourself in a terminal: if you have edited crates/ or wit/, \
             this rebuilds it for you, in the background, and reports the result here. \
             A build that fails restarts nothing and gives you the compiler error; a \
             binary that will not start is probed and rejected before it is adopted. \
             This turn continues afterwards unless you say otherwise; say why first, \
             because the restart happens just after your turn ends.",
        mutating: true,
        parameters: obj(
            json!({
                "reason": string_prop("Why a restart is needed."),
                "resume": {
                    "type": "boolean",
                    "description": "Carry this turn on once Thetis is back, which is the default. Set false only if the restart is the last thing you mean to do.",
                },
            }),
            &["reason"],
        ),
    }]
}

/// The isolated per-session container: running a command and moving files in and
/// out of its workspace.
///
/// A named group rather than an inline block in `available`, so that
/// `all_builtins` can add it back when the sandbox is unavailable. Without that
/// these three names were unclassifiable — `is_mutating` fell through to the
/// component path and defaulted them to mutating, and the group table's coverage
/// check reported them as tools that do not exist.
fn sandbox_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "exec",
            description: "Run a shell command in this session's isolated container.",
            mutating: true,
            parameters: obj(
                json!({
                    "command": string_prop("The command line to run."),
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds." },
                }),
                &["command"],
            ),
        },
        ToolDef {
            name: "write_file",
            description: "Write a file in the session's container workspace.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path inside the workspace."),
                    "contents": string_prop("Full file contents."),
                }),
                &["path", "contents"],
            ),
        },
        ToolDef {
            name: "read_file",
            description: "Read a file from the session's container workspace.",
            mutating: false,
            parameters: obj(json!({ "path": string_prop("Path inside the workspace.") }), &["path"]),
        },
    ]
}

/// Spawning and supervising sub-agents.
///
/// The descriptions carry more instruction than most, because delegation is the
/// tool surface where a vague call is most expensive: a badly briefed child
/// burns a whole conversation's worth of tokens before anyone finds out. The
/// research on multi-agent systems is consistent on the point — the dominant
/// failure is under-specification of the sub-task, not faulty execution of it —
/// so `spawn_agent` demands the objective, the output format and the
/// boundaries, and says why.
fn subagent_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "spawn_agent",
            description:
                "Delegate a self-contained piece of work to a sub-agent: a fresh conversation \
                 with its own context window, working in this same checkout, whose final answer \
                 comes back to you. Returns as soon as the child has started, so call it \
                 several times to fan out and then `wait`.\n\n\
                 Delegate when the work would otherwise flood your own context — reading a \
                 large body of code or documents to extract a little, exploring several \
                 approaches, or a batch of similar independent jobs. Do NOT delegate work that \
                 needs your conversation's history to make sense, work you could do in two tool \
                 calls, or anything needing a decision from the user: a sub-agent cannot ask \
                 questions and cannot delegate further.\n\n\
                 `task` is the entire briefing. The child cannot see this conversation, so a \
                 brief that assumes context produces confident, wrong work. Always state: the \
                 objective; what to return and in what shape; which files, paths or sources to \
                 use; and the boundaries — what it must not touch, and when to stop. Say what \
                 you already know so it does not rediscover it. Prefer a handful of well-briefed \
                 children over many thin ones.",
            mutating: true,
            parameters: obj(
                json!({
                    "label": string_prop(
                        "Two or three words naming this job, shown in the transcript and the \
                         UI, e.g. 'audit auth paths'."
                    ),
                    "task": string_prop(
                        "The complete brief: objective, deliverable and its format, where to \
                         look, boundaries, and what you already know. Assume no shared context."
                    ),
                    "profile": string_prop(
                        "Configured agent profile to run as, e.g. 'scout' or 'worker'. Omit for \
                         the default. Call agent_profiles to see them."
                    ),
                    "model": string_prop(
                        "Override the model, e.g. a cheaper one for bulk reading. Omit to \
                         inherit from the profile."
                    ),
                    "mode": string_prop(
                        "Override the mode. 'plan' gives a read-only child, which is the right \
                         choice for anything that only needs to look."
                    ),
                }),
                &["label", "task"],
            ),
        },
        ToolDef {
            name: "agent_status",
            description:
                "List your sub-agents with their state, answer so far, cost and elapsed time. \
                 A running child also reports how much log it has written and when it last \
                 wrote any, which is how you tell one that is working from one that is stuck. \
                 Free of side effects, but prefer `wait` when what you actually want is for one \
                 of them to be finished.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
        ToolDef {
            name: "agent_transcript",
            description:
                "Read a sub-agent's own event log — what it did, not just what it concluded. \
                 For diagnosing a child that failed or answered oddly; the answer in \
                 agent_status is the normal way to collect work.",
            mutating: false,
            parameters: obj(
                json!({
                    "child_id": string_prop("The sub-agent's id."),
                    "from_seq": {
                        "type": "integer",
                        "description": "Skip events before this sequence number. Omit for all.",
                    },
                }),
                &["child_id"],
            ),
        },
        ToolDef {
            name: "cancel_agent",
            description:
                "Stop a sub-agent that is no longer worth finishing — going the wrong way, or \
                 made redundant by another child's answer. Whatever it had produced is kept.",
            mutating: true,
            parameters: obj(
                json!({ "child_id": string_prop("The sub-agent's id.") }),
                &["child_id"],
            ),
        },
        ToolDef {
            name: "agent_profiles",
            description:
                "List the configured sub-agent profiles — each a model, a mode and a standing \
                 brief — and the delegation limits: how many children may run at once, the \
                 longest a wait may block, and how much of an answer reaches you.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// Recall: reading and searching past conversations and sub-agents.
///
/// Every one of these is read-only, and that is what justifies their reach.
/// Unlike the rest of the tool surface they see conversations this session did
/// not write — the host grants that precisely because nothing here can change
/// one. Say so in the descriptions, because a caller cannot tell from a schema
/// whose data it is about to read.
///
/// There are no separate sub-agent read/grep tools. A sub-agent *is* a session,
/// so its id goes straight into `conversation_read`, and one `include_subagents`
/// flag covers it in search. `agent_status` and `agent_transcript` remain a
/// different job: live supervision of the children this turn started.
fn transcript_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "conversation_list",
            description:
                "List conversations in this Thetis instance, most recently active first: id, \
                 title, mode, when it was last active, and a preview. Use it to find the \
                 conversation you want before reading or grepping it — and prefer \
                 `conversation_grep` when you know what you are looking for but not where.\n\n\
                 This sees every conversation, not only your own. Read-only.",
            mutating: false,
            parameters: obj(
                json!({
                    "include_archived": {
                        "type": "boolean",
                        "description": "Include archived conversations. Defaults to false.",
                    },
                    "include_subagents": {
                        "type": "boolean",
                        "description": "Include sub-agent sessions. Defaults to false: there \
                                        are usually far more of them than conversations. Use \
                                        subagent_list to see one conversation's children.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many to return, newest first. Omit for all of them.",
                    },
                }),
                &[],
            ),
        },
        ToolDef {
            name: "conversation_read",
            description:
                "Read a conversation's transcript by id — messages, tool calls, tool failures, \
                 notes and incidents, oldest first. Works on any conversation and on any \
                 sub-agent session, since a sub-agent is just a session: pass the child id from \
                 `subagent_list` or from a grep hit.\n\n\
                 Long entries arrive clipped, and the reply says how much was cut, so raise \
                 `max_chars` when you need a full message rather than assuming you have it. \
                 Page with `from_seq` set to the last seq you saw. Read-only.",
            mutating: false,
            parameters: obj(
                json!({
                    "session_id": string_prop(
                        "The conversation or sub-agent session id, from conversation_list, \
                         subagent_list or a conversation_grep hit."
                    ),
                    "from_seq": {
                        "type": "integer",
                        "description": "Skip events at or before this sequence number. Omit to \
                                        start at the beginning; set it to the last seq you saw \
                                        to page forward.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many entries to return. Omit for the default (200).",
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Characters per entry before clipping. Omit for the \
                                        default (600); raise it to read a long message in full.",
                    },
                }),
                &["session_id"],
            ),
        },
        ToolDef {
            name: "conversation_grep",
            description:
                "Search transcripts for a regular expression and get back the matching lines \
                 with the conversation and sequence number each came from. This is the tool for \
                 recall: whether you have hit a problem before, what was decided about something \
                 and where, which conversation a piece of work happened in.\n\n\
                 Searches every conversation by default — pass `session_id` to search just one. \
                 Successful tool output is skipped unless you ask for it, because file contents \
                 and command output are most of the bytes in a transcript and would bury the \
                 discussion; failed tool results are always searched, so error messages are \
                 findable. Newest conversation first, and the reply says plainly when the answer \
                 was capped. Read-only.",
            mutating: false,
            parameters: obj(
                json!({
                    "pattern": string_prop(
                        "Regular expression, Rust regex syntax. Prefix with (?i) to ignore case."
                    ),
                    "session_id": string_prop(
                        "Search only this conversation or sub-agent session. Omit to search all \
                         of them."
                    ),
                    "include_archived": {
                        "type": "boolean",
                        "description": "Search archived conversations too. Defaults to false.",
                    },
                    "include_subagents": {
                        "type": "boolean",
                        "description": "Search sub-agent logs too. Defaults to false. Worth \
                                        turning on when the work you are looking for was \
                                        delegated.",
                    },
                    "include_tool_output": {
                        "type": "boolean",
                        "description": "Also search successful tool output — file contents, \
                                        command output, other searches' results. Defaults to \
                                        false; it is a lot of noise, and a pattern matching a \
                                        file's text matches it in every conversation that ever \
                                        read that file.",
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Hits to return. Omit for the default (100).",
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Characters per hit before clipping. Omit for the default.",
                    },
                }),
                &["pattern"],
            ),
        },
        ToolDef {
            name: "subagent_list",
            description:
                "List the sub-agents spawned under a conversation — label, brief, state, when it \
                 last ran, and the child's session id, which `conversation_read` will open. Works for any \
                 conversation and reports the whole tree.\n\n\
                 For the children *you* started in this turn, `agent_status` is the better tool: \
                 it is live supervision, and `wait` blocks on it. This one is for looking at what \
                 some other conversation delegated. Read-only.",
            mutating: false,
            parameters: obj(
                json!({
                    "session_id": string_prop(
                        "The conversation whose sub-agents to list. Omit for this conversation."
                    ),
                }),
                &[],
            ),
        },
    ]
}

/// Reading and changing the grip's own settings.
///
/// Writes land in the config file with its comments intact and are refused
/// unless the result would still load; nothing takes effect until a restart.
fn configuration_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_config",
            description:
                "List Thetis's settings as dotted paths with their current values. Pass a \
                 prefix such as 'llm' or 'terminal' to narrow it.",
            mutating: false,
            parameters: obj(
                json!({ "prefix": string_prop("Section to narrow to, e.g. 'budgets'. Omit for all.") }),
                &[],
            ),
        },
        ToolDef {
            name: "read_config",
            description: "Read one setting by its dotted path, e.g. 'llm.model'.",
            mutating: false,
            parameters: obj(
                json!({ "key": string_prop("Dotted path of the setting.") }),
                &["key"],
            ),
        },
        ToolDef {
            name: "set_config",
            description:
                "Change one setting and write it back to the config file. The change is \
                 refused if the result would not load. Settings are read at startup, so call \
                 restart_orchestrator afterwards for it to take effect.",
            mutating: true,
            parameters: obj(
                json!({
                    "key": string_prop("Dotted path of the setting."),
                    "value": string_prop("New value. Its type is taken from the existing one."),
                }),
                &["key", "value"],
            ),
        },
    ]
}

/// Every tool the agent can call in this mode.
pub fn available(mode: &str) -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "remember",
            description:
                "Save a durable note for this conversation. Survives restarts and self-modification.",
            mutating: false,
            parameters: obj(
                json!({
                    "key": string_prop("Short identifier for the note."),
                    "value": string_prop("The content to remember."),
                }),
                &["key", "value"],
            ),
        },
        ToolDef {
            name: "recall",
            description:
                "Read back a saved note. Omit the key to list everything remembered here.",
            mutating: false,
            parameters: obj(
                json!({ "key": string_prop("The note to read; omit to list all keys.") }),
                &[],
            ),
        },
        // --- recovering a withheld tool group -------------------------------
        //
        // Always offered, even when grouping is off, so that the escape hatch
        // is not itself something that can be scoped away. With grouping off it
        // reports every group as loaded, which is true and harmless.
        //
        // Not mutating: it changes what this conversation can see, not anything
        // outside it, and a read-only session needs the escape hatch as much as
        // any other. The mode filter still applies to whatever it admits.
        ToolDef {
            name: TOOL_SEARCH,
            description:
                "Find and load a group of tools that is not currently in your tool list. Your \
                 tool surface is scoped to what this conversation looks like it needs, so \
                 capabilities you have — BigQuery, Notion, the web, GitHub, ssh hosts, the dev \
                 kit — may not be visible right now. Call this with a description of what you \
                 are trying to do, or with a group id, and the matching groups are added for \
                 the rest of the conversation. Nothing is ever removed. Call it the moment you \
                 suspect a tool exists but cannot see it, rather than working around the gap.",
            mutating: false,
            parameters: obj(
                json!({
                    "query": string_prop(
                        "What you need to do, in natural language, or a group id. Omit to list \
                         every group without loading anything."
                    ),
                    "load": {
                        "type": "array",
                        "description": "Group ids to load outright, skipping the ranking.",
                        "items": { "type": "string" },
                    },
                }),
                &[],
            ),
        },
        // --- asking the user ------------------------------------------------
        //
        // Deliberately not mutating: it changes nothing outside the
        // conversation, and a read-only mode is exactly where asking a
        // question matters most — it is the only thing such a session can do
        // besides read. That is also what lets Discord use it.
        ToolDef {
            name: ASK_USER,
            description:
                "Ask the user one or more questions and have them answered in the interface \
                 rather than in prose. Each question is either multiple choice or open ended; \
                 every choice question also offers a free-text answer of the user's own, and \
                 every question can be skipped. Use this whenever you need input to go on — a \
                 decision between options, a missing detail, a preference — and prefer it to \
                 writing questions into your reply, because the user gets something to click \
                 and the answers come back labelled. All the questions are presented at once. \
                 Calling this ends your turn: the answers arrive as the user's next message, so \
                 ask as soon as you need input rather than guessing and pressing on.",
            mutating: false,
            parameters: obj(
                json!({
                    "intro": string_prop(
                        "One line of context shown above the questions, e.g. why you are \
                         asking. Optional."
                    ),
                    "questions": {
                        "type": "array",
                        "description": "The questions, in the order they should be answered. At least one.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": string_prop(
                                    "Short key naming this question, used to label the answer \
                                     when it comes back, e.g. 'colour'. Optional."
                                ),
                                "question": string_prop("The question, worded as you would say it."),
                                "type": {
                                    "type": "string",
                                    "enum": ["choice", "open"],
                                    "description":
                                        "'choice' for multiple choice, 'open' for free text. \
                                         Defaults to 'choice' when options are given, 'open' \
                                         otherwise.",
                                },
                                "options": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description":
                                        "The choices, for a choice question. A free-text \
                                         'something else' box and a skip control are added for \
                                         you — do not put them in this list.",
                                },
                                "allow_multiple": {
                                    "type": "boolean",
                                    "description": "Let the user pick more than one option. Defaults to false.",
                                },
                            },
                            "required": ["question"],
                            "additionalProperties": false,
                        },
                    },
                }),
                &["questions"],
            ),
        },
        // --- waiting --------------------------------------------------------
        //
        // `wait` is core rather than part of the sub-agent group, because the
        // reason to block is not always delegation. A sub-agent is refused the
        // delegation tools entirely, and it is exactly the kind of session that
        // gets handed a long build to babysit — so if `wait` travelled with
        // that group, a child would have no way to sleep and would poll
        // instead, burning an iteration and a slice of context each time.
        //
        // The predicates that name sub-agents simply have nothing to match for
        // a session that has none; the host answers "no such sub-agents" and
        // the tool says so. Only the 'time' predicate is universally useful,
        // and it is the one every session can reach.
        ToolDef {
            name: "wait",
            description:
                "Block until something you are waiting on has happened, instead of polling in a \
                 loop — each round of polling costs an iteration and a slice of context.\n\n\
                 Predicates: 'time' — sleep for the timeout, for something outside this system, \
                 like a long build or a deploy settling; 'all' — every sub-agent named (or every \
                 one you have) has finished; 'any' — the first one finishes; 'first_failure' — \
                 return early if one fails, for a batch where one bad result invalidates the \
                 rest.\n\n\
                 Always returns the current state of all your sub-agents, including their \
                 answers, so a successful 'all' wait usually needs no follow-up call. A wait \
                 always has a deadline and reports whether it timed out.",
            // Waits, and cancels nothing. A read-only session still needs to be
            // able to sleep, and one that spawned a read-only child still needs
            // to be able to wait for it.
            mutating: false,
            parameters: obj(
                json!({
                    "until": {
                        "type": "string",
                        "enum": ["all", "any", "first_failure", "time"],
                        "description":
                            "What to wait for. Defaults to 'all' when sub-agents are named or \
                             running, and to 'time' otherwise.",
                    },
                    "children": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description":
                            "Sub-agent ids to watch. Omit for all of yours.",
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description":
                            "Deadline in seconds. Required for 'time'; otherwise a safety net \
                             — pick a generous one, since returning early loses nothing.",
                    },
                }),
                &[],
            ),
        },
        // --- skills ---------------------------------------------------------
        //
        // The system prompt names skills but never carries their instructions.
        // These are how the body is actually obtained, so the descriptions have
        // to be pushy: a skill that is never fetched may as well not exist.
        ToolDef {
            name: "skill_fetch",
            description:
                "Read a skill's full instructions before doing the thing it covers. The system \
                 prompt lists only one-line briefs, so fetch the skill whenever its brief looks \
                 relevant — the brief is a pointer, not the content. Also fetches a bundled \
                 reference, script or asset by relative path.",
            mutating: false,
            parameters: obj(
                json!({
                    "id": string_prop("Skill id, e.g. 'careful-surgery' or 'careful-surgery/contract-changes'."),
                    "file": string_prop(
                        "Bundled file to read instead of the body, e.g. 'references/format.md'. \
                         Omit for the skill's own instructions."
                    ),
                    "offset": { "type": "integer", "description": "Character offset to start at, for a long body." },
                    "limit": { "type": "integer", "description": "Characters to return; omit for as much as fits." },
                }),
                &["id"],
            ),
        },
        ToolDef {
            name: "skill_search",
            description:
                "Search the skill corpus by meaning and get back matching briefs, ranked. Use \
                 this when a task looks like something there might be a skill for but nothing \
                 in the prompt names it — the prompt only carries universal skills plus what \
                 was retrieved for the opening message, so most of the corpus is not listed.",
            mutating: false,
            parameters: obj(
                json!({
                    "query": string_prop("What you are trying to do, in natural language."),
                    "limit": { "type": "integer", "description": "How many to return; omit for the configured default." },
                }),
                &["query"],
            ),
        },
        ToolDef {
            name: "skill_write",
            description:
                "Create or replace a skill, or one of its bundled files. Replaces the whole \
                 file, as with code edits. Lint diagnostics come back in the same call. Use \
                 this to record a procedure worth keeping rather than repeating it from memory \
                 next time.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop(
                        "Skill id. A '/' nests it under a parent, e.g. 'careful-surgery/contract-changes'."
                    ),
                    "file": string_prop(
                        "Bundled file to write instead of the skill itself, e.g. \
                         'references/format.md'. Omit to write the skill's own SKILL.md."
                    ),
                    "contents": string_prop(
                        "Full file contents. A skill needs TOML frontmatter with at least \
                         name and brief; call skill_fetch on 'skill-creator' for the format."
                    ),
                }),
                &["id", "contents"],
            ),
        },
        ToolDef {
            name: "skill_delete",
            description:
                "Delete a skill. Refuses a skill with nested children unless recursive is set, \
                 so a subtree cannot be orphaned by accident.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop("Skill id to delete."),
                    "recursive": { "type": "boolean", "description": "Also delete nested skills beneath it." },
                }),
                &["id"],
            ),
        },
        ToolDef {
            name: "skill_lint",
            description:
                "Check skills for problems: missing or overlong briefs, broken parent links, \
                 nesting deeper than the limit. Omit the id to lint the whole corpus.",
            mutating: false,
            parameters: obj(
                json!({ "id": string_prop("Skill to lint; omit for the whole tree.") }),
                &[],
            ),
        },
    ];

    // Offered in every mode, not only Plan. A plan written while reading is
    // worth having in Agent mode too, and Agent mode is where it gets executed
    // and ticked off — so gating these on the mode would mean the executing
    // session could not record that a step was done.
    tools.extend(plan_tools());
    // A todo list is the same kind of conversation-local artefact as a plan,
    // and must remain readable and writable in every mode.
    tools.extend(todo_tools());

    if sandbox_available() {
        tools.extend(sandbox_tools());
    }

    if devkit_available() {
        tools.extend(devkit_tools());
    }
    if filesystem_available() {
        tools.extend(filesystem_tools());
    }
    if terminal_available() {
        tools.extend(terminal_tools());
        // git_clone runs the credential script in a shell, so it is offered on
        // the same condition as the terminal itself.
        tools.extend(git_tools());
        if terminal::ssh_available() {
            tools.extend(ssh_host_tools());
        }
    }
    if delegation_available() {
        tools.extend(subagent_tools());
    }
    // No capability flag: the event log is always there, and these only read it.
    // A sub-agent gets them too — it is refused `subagent_tools` because it may
    // not *spawn*, which says nothing about whether it may read. A child sent to
    // investigate something is exactly the session most likely to need recall,
    // and it cannot ask its parent for context mid-turn.
    tools.extend(transcript_tools());
    tools.extend(configuration_tools());
    if restart_available() {
        tools.extend(restart_tools());
    }

    // In a read-only mode the tools that would change something are simply not
    // offered, rather than offered and then refused.
    if read_only(mode) {
        tools.retain(|t| !t.mutating);
    }

    tools
}

/// The plan document: one editable artefact per conversation.
///
/// None of these is `mutating`, and that is the deliberate part. The mode filter
/// asks "does this change something outside the conversation?", and a plan is
/// this conversation's own notes — the same category as `remember`, which is
/// declared the same way. Withholding them from Plan mode would leave the one
/// mode whose entire output is a plan unable to write one down.
///
/// A named group rather than an inline block in `available`, so `all_builtins`
/// can add them back for classification. See the note on `restart_tools`.
fn plan_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "plan_write",
            description:
                "Create or replace this conversation's plan document. The plan opens in its own \
                 tab, where the user can read it and press Execute to hand it to an agent — so \
                 write it for that reader: numbered steps, the files each one touches, and the \
                 decisions that are theirs to make. Prefer `plan_edit` for a revision; a whole \
                 rewrite is how an approved section quietly disappears.",
            mutating: false,
            parameters: obj(
                json!({
                    "body": string_prop(
                        "The plan, in markdown. Headings and numbered steps render in the plan tab."
                    ),
                    "title": string_prop(
                        "Short title for the tab. Omit to keep the existing one."
                    ),
                }),
                &["body"],
            ),
        },
        ToolDef {
            name: "plan_edit",
            description:
                "Revise part of the plan by replacing an exact snippet — the same contract as \
                 `edit_path`. Read the plan first: `old_text` must match byte for byte, \
                 whitespace included, and an edit matching nothing or matching twice is refused \
                 rather than guessed. This is the tool for reworking a step after the user \
                 pushes back on it.",
            mutating: false,
            parameters: obj(
                json!({
                    "old_text": string_prop(
                        "Exact text to find in the plan; must appear once unless replace_all is set."
                    ),
                    "new_text": string_prop("What to put in its place. Empty deletes the snippet."),
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring a unique one.",
                    },
                }),
                &["old_text", "new_text"],
            ),
        },
        ToolDef {
            name: "plan_append",
            description:
                "Add a section to the end of the plan, without restating the rest of it. Use \
                 this while investigating, as each part of the shape becomes clear.",
            mutating: false,
            parameters: obj(
                json!({ "text": string_prop("Markdown to add at the end.") }),
                &["text"],
            ),
        },
        ToolDef {
            name: "plan_read",
            description:
                "Read this conversation's plan back, with its revision number. Do this before \
                 editing: another turn may have revised it, and `plan_edit` matches exact text.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// A concise, durable progress list for this conversation.
///
/// These tools are deliberately not mutating: like the plan document, they
/// change only conversation state and must remain available in read-only modes.
fn todo_tools() -> Vec<ToolDef> {
    let item = json!({
        "type": "object",
        "properties": {
            "content": string_prop("Short imperative description of the work."),
            "active_form": string_prop("Present-continuous label shown while in progress, e.g. 'Testing login'."),
            "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] },
        },
        "required": ["content"],
        "additionalProperties": false,
    });
    vec![
        ToolDef {
            name: "todo_write",
            description: "Write this conversation's todo list, replacing whatever is there. Use it once when work has three or more distinct steps or the user gives you a list. Prefer todo_update afterwards: restating the whole list to change one status is how a row quietly disappears.",
            mutating: false,
            parameters: obj(json!({ "todos": { "type": "array", "items": item.clone(), "description": "The complete replacement list." } }), &["todos"]),
        },
        ToolDef {
            name: "todo_add",
            description: "Add items without restating the list. Use this when work turns out to be larger than expected or a blocker becomes concrete.",
            mutating: false,
            parameters: obj(json!({ "todos": { "type": "array", "items": item, "description": "Items to append." } }), &["todos"]),
        },
        ToolDef {
            name: "todo_update",
            description: "Change items in place. Mark exactly one item in_progress before starting it and completed when it is actually done. Use cancelled for work deliberately abandoned and say why in your reply.",
            mutating: false,
            parameters: obj(json!({
                "updates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": string_prop("Stable todo id, e.g. t3. Preferred: it survives the list being reordered."),
                            "index": { "type": "integer", "description": "One-based item position, for when you do not have the id. Sending both is fine — the id wins." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] },
                            "content": string_prop("Replacement description."),
                            "active_form": string_prop("Replacement present-continuous label."),
                        },
                        "additionalProperties": false,
                    },
                }
            }), &["updates"]),
        },
        ToolDef {
            name: "todo_read",
            description: "Read the authoritative todo list. Use this after a long stretch of tool work or context compaction if you have lost track of it.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// Reading and writing files on the machine Thetis is running on, confined to
/// the roots named in configuration.
fn filesystem_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_path",
            description:
                "Read a file from the host filesystem, with every line numbered. Reads the \
                 whole file by default; for a large one pass `offset` and `limit` to take a \
                 window, and the reply says how to read on. Prefer this over `cat`, `head`, \
                 `sed -n` or `less` in a terminal: it costs fewer tokens and the line numbers \
                 are what let you anchor an `edit_path` afterwards.",
            mutating: false,
            parameters: obj(
                json!({
                    "path": string_prop("Path, relative to the project root unless absolute."),
                    "offset": {
                        "type": "integer",
                        "description": "First line to read, counting from 1. Omit to start at the top.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many lines to read. Omit for as many as fit.",
                    },
                }),
                &["path"],
            ),
        },
        ToolDef {
            name: "edit_path",
            description:
                "Change part of a file on the host filesystem by replacing an exact snippet. \
                 This is the tool for an ordinary edit — reach for `write_path` only to create \
                 a file or genuinely rewrite all of it. Read the file first: `old_text` must \
                 match byte for byte, indentation included. If it appears more than once the \
                 edit is refused, so add surrounding lines until it is unique, or pass \
                 `replace_all`. Prefer this over `sed -i`, `python -c` or a heredoc in a \
                 terminal: those cost far more tokens, break on quoting, and are recorded as \
                 someone else's edit rather than yours.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path to edit."),
                    "old_text": string_prop("Exact text to find; must appear exactly once unless replace_all is set."),
                    "new_text": string_prop("What to put in its place. Empty deletes the snippet."),
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring a unique one. Use for a rename.",
                    },
                }),
                &["path", "old_text", "new_text"],
            ),
        },
        ToolDef {
            name: "search_files",
            description:
                "Search file contents across the tree for a regular expression, and get back \
                 `path:line: text` for each hit. This is how to find where something lives: \
                 a symbol's definition, every caller of a function, a string from a log. \
                 Narrow with `glob` ('*.rs') or `path` (a subdirectory) rather than widening \
                 the pattern. Use mode='files' when you only need to know which files are \
                 involved, which is much cheaper on a common word. Build output, `.git` and \
                 vendored directories are skipped. Prefer this over `grep`, `rg` or `find` in \
                 a terminal — it is bounded, and it tells you when the answer is partial.",
            mutating: false,
            parameters: obj(
                json!({
                    "pattern": string_prop("Regular expression, Rust regex syntax. Prefix with (?i) to ignore case."),
                    "path": string_prop(
                        "Where to search: a directory, or a single file to search just that one. \
                         Omit for the project root.",
                    ),
                    "glob": string_prop("Only search files whose name matches, e.g. '*.rs' or 'src/**/*.toml'."),
                    "mode": {
                        "type": "string",
                        "enum": ["content", "files", "count"],
                        "description": "content (default) gives matching lines, files gives just the paths, count gives a tally per file.",
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Stop after this many. Omit for a sensible bound.",
                    },
                }),
                &["pattern"],
            ),
        },
        ToolDef {
            name: "find_files",
            description:
                "List files whose path matches a glob, most recently changed first. Use it to \
                 find a file you know the shape of the name of, or to see what a directory \
                 holds without listing it level by level. A pattern with no '/' matches the \
                 file name anywhere in the tree, so '*.wit' finds every one. Prefer this over \
                 `find` or `ls -R` in a terminal.",
            mutating: false,
            parameters: obj(
                json!({
                    "glob": string_prop("Glob such as '*.rs', 'crates/**/*.toml', or 'Cargo.*'."),
                    "path": string_prop(
                        "Where to look: a directory, or a single file to test just that one. \
                         Omit for the project root.",
                    ),
                    "max_results": {
                        "type": "integer",
                        "description": "How many to return. Omit for a sensible bound.",
                    },
                }),
                &["glob"],
            ),
        },
        ToolDef {
            name: "write_path",
            description:
                "Create a file on the host filesystem, or replace one outright, creating parent \
                 directories as needed. This writes the whole file, so use `edit_path` for a \
                 change to a file that already exists — a whole-file write costs the file's \
                 length in tokens and loses anything you did not think to repeat.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path to write."),
                    "contents": string_prop("The complete new file contents."),
                }),
                &["path", "contents"],
            ),
        },
        ToolDef {
            name: "list_path",
            description:
                "List one directory on the host filesystem. To look across a whole tree use \
                 `find_files` instead of walking level by level.",
            mutating: false,
            parameters: obj(
                json!({ "path": string_prop("Directory to list; '.' for the project root.") }),
                &["path"],
            ),
        },
        ToolDef {
            name: "delete_path",
            description:
                "Delete a file or directory on the host filesystem. This cannot be undone, so \
                 check the path first.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path to delete."),
                    "recursive": {
                        "type": "boolean",
                        "description": "Required to delete a directory that is not empty.",
                    },
                }),
                &["path"],
            ),
        },
    ]
}

/// Shell sessions. A session keeps its working directory and shell state
/// between commands, which is the point of opening one rather than running a
/// series of unrelated commands.
fn terminal_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "terminal_open",
            description:
                "Open a shell session and return its id. Reuse the id for related commands so \
                 the working directory and environment carry over. Pass `host` to open the \
                 session on a remote machine over ssh, naming a host you registered with \
                 `ssh_host_set`.\n\nThe terminal is for running programs — builds, tests, git, \
                 package managers, processes, anything whose output you need. It is not the way \
                 to look at or change files: `search_files`, `find_files`, `read_path` and \
                 `edit_path` do that far more cheaply and reliably than grep, sed and heredocs, \
                 and edits made through the shell are attributed to the user rather than to you.",
            mutating: true,
            parameters: obj(
                json!({
                    "cwd": string_prop(
                        "Directory to start in; defaults to the project root. Ignored for a \
                         remote session, which starts where its host says.",
                    ),
                    "host": string_prop(
                        "Name of a registered ssh host, to open the session on that machine \
                         instead of this one. Omit for a local shell. List the names with \
                         `ssh_host_list`.",
                    ),
                    "name": string_prop(
                        "A label for this session, so listings read as something better than \
                         term-1. Optional.",
                    ),
                    "env": {
                        "type": "object",
                        "description": "Environment variables for the session's shell, as a \
                                        flat name-to-value object.",
                        "additionalProperties": { "type": "string" },
                    },
                }),
                &[],
            ),
        },
        ToolDef {
            name: "terminal_run",
            description:
                "Run a command in an open shell session and wait for it to finish. Returns \
                 whatever it printed, with the exit status. A command that outlives the timeout \
                 keeps running; read the session again later for the rest.\n\nThis waits for the \
                 command to complete, so it cannot drive anything interactive: a program sitting \
                 at a prompt never finishes. Use `terminal_send` for that, and `terminal_signal` \
                 to interrupt.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop("Session id from terminal_open."),
                    "command": string_prop("The command line to run."),
                    "timeout_ms": {
                        "type": "integer",
                        "description": "How long to wait. Omit for the configured default.",
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Start the command and return at once instead of waiting. \
                                        Use for something long whose output you want later — a \
                                        build, a test suite, a server. Collect it with \
                                        `terminal_read`. The session will not accept another \
                                        command until it ends, so open a second session if you \
                                        need to work meanwhile.",
                    },
                }),
                &["id", "command"],
            ),
        },
        ToolDef {
            name: "terminal_read",
            description:
                "Read anything a session has printed since the last read, without running \
                 anything. Use it to collect output from a command that timed out or was \
                 backgrounded — it says whether that command has since finished.",
            mutating: false,
            parameters: obj(json!({ "id": string_prop("Session id.") }), &["id"]),
        },
        ToolDef {
            name: "terminal_send",
            description:
                "Write raw input to a session and return immediately, without waiting for a \
                 command to complete. This is how to answer a prompt — a confirmation, a \
                 passphrase, a `y` — or to drive a REPL, none of which `terminal_run` can do, \
                 because it waits for the command to end and an interactive program never \
                 ends.\n\nWhatever the program printed in the moment after is returned; use \
                 `terminal_read` for the rest.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop("Session id."),
                    "text": string_prop(
                        "The text to write. Pass an empty string with submit to send a bare \
                         newline.",
                    ),
                    "submit": {
                        "type": "boolean",
                        "description": "Append a newline, as pressing Enter would. Defaults to \
                                        true; set false to send a partial line or a control \
                                        sequence.",
                    },
                }),
                &["id", "text"],
            ),
        },
        ToolDef {
            name: "terminal_signal",
            description:
                "Interrupt what a session is running, leaving the session itself alive — the \
                 deliberate Ctrl-C. Use it to stop a `tail -f`, end a test run whose first \
                 failure already told you what you needed, or unstick a command waiting on \
                 input you cannot give.\n\nA remote session needs a pty on its host for this to \
                 be deliverable, and can then take only INT, QUIT and TSTP.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop("Session id."),
                    "signal": {
                        "type": "string",
                        "description": "Which signal to send. INT is the usual one.",
                        "enum": ["INT", "TERM", "TSTP", "HUP", "QUIT", "KILL"],
                    },
                }),
                &["id", "signal"],
            ),
        },
        ToolDef {
            name: "terminal_close",
            description: "Close a shell session and stop its process.",
            mutating: true,
            parameters: obj(json!({ "id": string_prop("Session id.") }), &["id"]),
        },
        ToolDef {
            name: "terminal_list",
            description:
                "List the open shell sessions, with where each one runs, where it is, and \
                 whether it is busy with a background command.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// Where a clone lands unless the caller names somewhere else.
///
/// `workspace/` is gitignored, so a clone does not show up as untracked cruft
/// in the conversation's own worktree — which is what would happen if a repo of
/// any size were dropped at the root.
const CLONE_ROOT: &str = "workspace";

/// A real working tree, for the jobs the GitHub REST API cannot do.
///
/// The `git-*` tools cover reads, commits and PRs without a clone, and that
/// stays the better path. This is for a rebase, a bisect, or running a test
/// suite — anything that needs files and a `.git`. It shells out to
/// `scripts/github-app-git.sh`, which mints an installation token from the same
/// `[tools.git]` credential the tools use, so a clone authenticates and commits
/// as the app's own `[bot]` identity with no token left on disk.
///
/// Offered only when the terminal is, since that is how it runs.
fn git_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "git_clone",
        description:
            "Clone a GitHub repository into a real working tree, authenticated as the app's \
             GitHub App identity. Use this when you need actual files and a .git — a rebase, a \
             bisect, running a test suite, or a commit series with exact parentage; the git-* \
             tools read and write through the API without cloning, which is cheaper and is \
             still the right default. Lands in workspace/<name> unless you say otherwise, and \
             workspace/ is gitignored, so the clone does not pollute your own checkout. The \
             clone's remote is left tokenless and user.name/user.email are set to the [bot] \
             identity; to push later, run: eval \"$(scripts/github-app-git.sh env)\" in a \
             terminal session first, because the installation token lasts an hour.",
        mutating: true,
        parameters: obj(
            json!({
                "repo": string_prop("Repository as `owner/repo`, or its URL."),
                "dir": string_prop(
                    "Where to put it, relative to your checkout. Defaults to \
                     workspace/<repo-name>."),
                "ref": string_prop(
                    "Branch, tag or commit to check out after cloning. Defaults to the \
                     repository's default branch."),
                "depth": {
                    "type": "integer",
                    "description": "Truncate history to this many commits. Omit for the full \
                                    history, which is what a rebase or a bisect needs.",
                },
            }),
            &["repo"],
        ),
    }]
}

/// The named-host registry, split one tool per operation, offered only when
/// remote sessions are actually
/// possible — tools for managing hosts nothing can connect to are noise.
///
/// Connection details live here once and are referred to by name, so a session
/// is opened with `host: "build-box"` rather than a line of ssh arguments.
///
/// One tool per operation rather than one tool with an `action` enum: the two
/// readers can then be offered in a read-only mode, which a single mutating
/// mega-tool could not be, each schema requires exactly the fields its own
/// operation needs, and a mistyped action becomes an unknown tool name instead
/// of a runtime error.
fn ssh_host_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "ssh_host_list",
            description:
                "List the named ssh hosts `terminal_open` can open a session on, with each \
                 one's connection details. Start here when you need a host name.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
        ToolDef {
            name: "ssh_host_get",
            description:
                "Show one registered ssh host's connection details. Use `ssh_host_list` when \
                 you do not know the name.",
            mutating: false,
            parameters: obj(
                json!({ "name": string_prop("The host's name.") }),
                &["name"],
            ),
        },
        ToolDef {
            name: "ssh_host_set",
            description:
                "Register a new ssh host for `terminal_open`, or edit one that exists. Fields \
                 you leave out keep their current value, so this is how to change just the port \
                 or just the pty setting.\n\nA name here is all `terminal_open` needs, so \
                 connection details are stated once. Hosts live in a gitignored file the config \
                 loader never reads: it cannot be published by accident, it never appears in \
                 `list_config`, and a bad entry cannot stop Thetis from starting. Only the \
                 *path* to a private key is stored, never a key or a password — ssh runs with \
                 BatchMode on and will not prompt, so a host must authenticate by key.",
            mutating: true,
            parameters: obj(
                json!({
                    "name": string_prop(
                        "The host's name, as `terminal_open` will refer to it. Letters, digits, \
                         '-', '_' and '.'.",
                    ),
                    "host": string_prop("Hostname or address. Required when first adding."),
                    "port": {
                        "type": "integer",
                        "description": "Port. Omit to let ssh decide, which respects ~/.ssh/config.",
                    },
                    "user": string_prop("Login user. Omit to let ssh decide."),
                    "identity_file": string_prop(
                        "Path to the private key to authenticate with. Omit for ssh's own key \
                         discovery.",
                    ),
                    "options": {
                        "type": "array",
                        "description": "Extra ssh arguments, each complete, e.g. \
                                        \"-oStrictHostKeyChecking=accept-new\".",
                        "items": { "type": "string" },
                    },
                    "remote_cwd": string_prop(
                        "Directory to enter on connecting. A session refuses to start if it does \
                         not exist, rather than landing in the login directory.",
                    ),
                    "pty": {
                        "type": "boolean",
                        "description": "Allocate a remote terminal. Needed for `terminal_signal` \
                                        to reach that host, and for anything that demands a tty \
                                        such as sudo. Costs echoed commands and prompts mixed \
                                        into output, so leave it off unless you need it.",
                    },
                    "description": string_prop("A note to yourself about what this host is."),
                }),
                &["name"],
            ),
        },
        ToolDef {
            name: "ssh_host_remove",
            description:
                "Delete a registered ssh host. Sessions already open on it keep running; \
                 nothing new can be opened by that name afterwards.",
            mutating: true,
            parameters: obj(
                json!({ "name": string_prop("The host's name.") }),
                &["name"],
            ),
        },
        ToolDef {
            name: "ssh_host_rename",
            description:
                "Rename a registered ssh host, keeping its connection details. The old name \
                 stops working, so update anything that refers to it.",
            mutating: true,
            parameters: obj(
                json!({
                    "name": string_prop("The host's current name."),
                    "to": string_prop("The new name. Letters, digits, '-', '_' and '.'."),
                }),
                &["name", "to"],
            ),
        },
    ]
}

/// Tools that edit the running system. Every mutating one rebuilds immediately
/// and returns the compiler's verdict in its result.
fn devkit_tools() -> Vec<ToolDef> {
    let target_prop = json!({
        "type": "string",
        "description": "What to edit: 'self' for your own loop, 'gateway:<name>' for a chat \
                        interface, or 'tool:<name>' for one of your tools. Editing a gateway \
                        changes only YOUR copy: the interface every browser loads is trunk's \
                        until your work is merged. To see your own version, open \
                        /preview/<this session id>/ — it serves your build against the real \
                        running system. Never start a second Thetis to look at a UI change.",
    });

    vec![
        ToolDef {
            name: "new_tool",
            description:
                "Scaffold a new tool component: creates the crate, builds it, and loads it. \
                 Returns the compile result.",
            mutating: true,
            parameters: obj(
                json!({
                    "name": string_prop("Lowercase name, hyphens allowed."),
                    "description": string_prop("What the tool does; shown to you later."),
                }),
                &["name", "description"],
            ),
        },
        ToolDef {
            name: "write_code",
            description:
                "Replace a whole file in a component, then rebuild and hot-swap it. The compile \
                 result comes back immediately, so fix errors and call again until it builds.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "path": string_prop("File path within the component's source tree."),
                    "contents": string_prop("The complete new file contents."),
                }),
                &["target", "path", "contents"],
            ),
        },
        ToolDef {
            name: "patch_code",
            description:
                "Replace an exact snippet in a component file, then rebuild and hot-swap it. \
                 Returns the compile result.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "path": string_prop("File path within the component's source tree."),
                    "old_text": string_prop("Exact text to find; must appear exactly once."),
                    "new_text": string_prop("Replacement text."),
                }),
                &["target", "path", "old_text", "new_text"],
            ),
        },
        ToolDef {
            name: "add_dependency",
            description:
                "Add a crate from crates.io to a component's dependencies, then rebuild it. Any \
                 published crate that supports wasm32-wasip2 will work; pure-computation crates \
                 almost always do. This fetches over the network, so it is slower than an \
                 ordinary edit. If the build fails the manifest is put back as it was.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "name": string_prop("Crate name as published, e.g. 'regex'."),
                    "version": string_prop("Version requirement, e.g. '1' or '0.4.31'."),
                    "features": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Cargo features to enable. Omit for none.",
                    },
                    "default_features": {
                        "type": "boolean",
                        "description": "Keep the crate's default features. Defaults to true.",
                    },
                }),
                &["target", "name", "version"],
            ),
        },
        ToolDef {
            name: "remove_dependency",
            description: "Drop a crate from a component's dependencies and rebuild it.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "name": string_prop("Crate name to remove."),
                }),
                &["target", "name"],
            ),
        },
        ToolDef {
            name: "list_dependencies",
            description: "List the crates a component currently depends on.",
            mutating: false,
            parameters: obj(json!({ "target": target_prop }), &["target"]),
        },
        ToolDef {
            name: "read_code",
            description: "Read one of your own source files.",
            mutating: false,
            parameters: obj(
                json!({ "target": target_prop, "path": string_prop("File path to read.") }),
                &["target", "path"],
            ),
        },
        ToolDef {
            name: "list_code",
            description: "List the source files of a component.",
            mutating: false,
            parameters: obj(json!({ "target": target_prop }), &["target"]),
        },
        ToolDef {
            name: "branch_status",
            description:
                "Where this conversation's sandbox branch stands: what changed here, whether \
                 trunk has moved on, and any unresolved merge conflicts.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
        ToolDef {
            name: "branch_log",
            description:
                "The commit history of this conversation's branch — every green build, skill \
                 edit, and checkpoint, newest first.",
            mutating: false,
            parameters: obj(
                json!({
                    "limit": { "type": "integer", "description": "How many commits (default 20)." },
                }),
                &[],
            ),
        },
        ToolDef {
            name: "update_from_trunk",
            description:
                "Bring the latest trunk into this conversation's branch. Fast-forwards when \
                 possible; a conflicted merge comes back with the conflicted files listed and \
                 standard git conflict markers left in them — resolve with the normal editing \
                 tools, then call complete_merge.",
            mutating: true,
            parameters: obj(json!({}), &[]),
        },
        ToolDef {
            name: "reset_branch",
            description:
                "Restore this conversation's code to how it was at an earlier commit (see \
                 branch_log). Use when a change made things worse. Moves forward: history is \
                 kept, nothing is rewritten.",
            mutating: true,
            parameters: obj(
                json!({
                    "rev": { "type": "string", "description": "The commit to restore to." },
                }),
                &["rev"],
            ),
        },
        ToolDef {
            name: "complete_merge",
            description:
                "Finish a conflicted trunk update after resolving every conflict marker in the \
                 working tree. Refuses while markers remain.",
            mutating: true,
            parameters: obj(
                json!({
                    "message": { "type": "string", "description": "Optional merge commit message." },
                }),
                &[],
            ),
        },
        ToolDef {
            name: "abort_merge",
            description: "Abandon a conflicted trunk update, restoring the pre-merge state.",
            mutating: true,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// Every tool offered in this mode, described for a human reader rather than
/// for the model. Built-ins are tagged so the panel can group them, and
/// mutating ones are tagged so it can explain what a read-only mode withholds.
pub fn manifests(mode: &str) -> Vec<ToolManifest> {
    let mut out: Vec<ToolManifest> = available(mode)
        .iter()
        .map(|t| ToolManifest {
            name: t.name.to_string(),
            description: t.description.to_string(),
            args_schema_json: t.parameters.to_string(),
            capabilities: {
                let mut caps = vec!["built-in".to_string()];
                if t.mutating {
                    caps.push("mutating".to_string());
                }
                caps
            },
        })
        .collect();

    // Mirrors `definitions`: in a read-only mode only self-declared read-safe
    // components appear, so the panel cannot claim a tool the model is not
    // actually given.
    let ro = read_only(mode);
    for mut manifest in tooling::registry() {
        if ro && !manifest.capabilities.iter().any(|c| c == READ_ONLY_CAP) {
            continue;
        }
        manifest.capabilities.push("component".to_string());
        out.push(manifest);
    }
    out
}

/// Tool definitions in the format the chat completions API expects, including
/// any hot-loaded tool components the orchestrator has registered, restricted
/// to an active set of tool groups.
///
/// `None` means every group, which is the behaviour before grouping existed and
/// what runs whenever `tool_groups.grouping_enabled` is off. A group filter is
/// applied *after* the mode filter, never instead of it: scoping is about what
/// is likely wanted, read-only modes are about what is permitted, and confusing
/// the two would let a task-shaped signal hand back a mutating tool.
pub fn definitions_for(mode: &str, active: Option<&[String]>) -> Vec<Value> {
    let mut defs: Vec<Value> = available(mode)
        .iter()
        .filter(|t| match active {
            Some(active) => groups::builtin_active(t.name, active),
            None => true,
        })
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect();

    // Hot-loaded tools are opaque: a read-only mode cannot tell what they do,
    // so it offers only those that declare themselves read-safe. A tool that
    // says nothing is still withheld.
    let ro = read_only(mode);

    for manifest in tooling::registry() {
        if ro && !manifest.capabilities.iter().any(|c| c == READ_ONLY_CAP) {
            continue;
        }
        // The manifest is already in hand, so its group is read from it rather
        // than looked up again through the registry it came from.
        if let Some(active) = active {
            let group = groups::component_group(&manifest.name, &manifest.capabilities);
            if !active.iter().any(|a| *a == group) {
                continue;
            }
        }
        let parameters = serde_json::from_str::<Value>(&manifest.args_schema_json)
            .unwrap_or_else(|_| obj(json!({}), &[]));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": manifest.name,
                "description": manifest.description,
                "parameters": parameters,
            },
        }));
    }

    defs
}

/// Every built-in name the agent could offer, for the group-coverage check.
pub fn builtin_names() -> Vec<String> {
    all_builtins()
        .iter()
        .map(|t| t.name.to_string())
        .collect()
}

// --- dispatch ---------------------------------------------------------------

pub fn invoke(
    session_id: &str,
    mode: &str,
    name: &str,
    args_json: &str,
) -> Result<String, String> {
    // Withholding a tool from the definitions is not enough on its own: a model
    // can still name one it saw earlier in the conversation. The mode is
    // enforced here, where the call would actually happen.
    if read_only(mode) && is_mutating(name) {
        return Err(format!(
            "'{name}' changes things, and this conversation is in {mode} mode. Switch to agent mode to run it."
        ));
    }

    // A tool called from outside the active groups is honoured, and its group is
    // admitted on the way through. Scoping is an attention and token
    // optimisation, never a permission boundary — the permission boundary is
    // the mode check above, and it has already run. If the model knows a tool
    // exists (from earlier in the conversation, from a skill body, from the
    // group catalogue) then refusing it would turn a saving into a capability
    // loss, which is the one failure this whole design is arranged to avoid.
    if groups::grouping_enabled() {
        let active = groups::active(session_id);
        if !groups::is_active(name, &active) {
            let group = groups::group_of(name);
            groups::admit(session_id, &[group.clone()]);
            sys::log(
                LogLevel::Info,
                &format!("'{name}' was called from outside the active groups; admitted '{group}'"),
            );
        }
    }

    let args: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("arguments were not valid JSON: {e}"))?;

    match name {
        "remember" => remember(session_id, &args),
        "recall" => recall(session_id, &args),
        ASK_USER => ask_user(&args),
        TOOL_SEARCH => tool_search(session_id, &args),

        "skill_fetch" => skills::fetch(
            &req_str(&args, "id")?,
            &opt_str(&args, "file"),
            args.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32,
            args.get("limit").and_then(Value::as_u64).unwrap_or(0) as u32,
        )
        .map(format_skill_body),
        "skill_search" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(0) as u32;
            Ok(format_skill_cards(&skills::search(
                &req_str(&args, "query")?,
                limit,
            )))
        }
        "skill_write" => skills::upsert(
            &req_str(&args, "id")?,
            &opt_str(&args, "file"),
            &req_str(&args, "contents")?,
        )
        .map(format_skill_write),
        "skill_delete" => skills::remove(
            &req_str(&args, "id")?,
            args.get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        "skill_lint" => {
            let diags = skills::lint(&opt_str(&args, "id"));
            if diags.is_empty() {
                return Ok("lint clean".to_string());
            }
            Ok(format_skill_diagnostics(&diags))
        }

        "plan_write" => plan::write(
            session_id,
            args.get("title").and_then(Value::as_str).map(str::to_string),
            &req_str(&args, "body")?,
        )
        .map(|p| {
            format!(
                "plan saved at revision {}. It is open in the Plan tab; the user can press \
                 Execute there to hand it to an agent.\n\n{}",
                p.revision,
                plan::describe(&p)
            )
        }),
        "plan_edit" => plan::edit(
            session_id,
            &req_str(&args, "old_text")?,
            &req_str(&args, "new_text")?,
            args.get("replace_all").and_then(Value::as_bool).unwrap_or(false),
        )
        .map(|e| {
            format!(
                "plan edited in {} place(s), now revision {}.\n\n{}",
                e.replacements,
                e.plan.revision,
                plan::describe(&e.plan)
            )
        }),
        "plan_append" => plan::append(session_id, &req_str(&args, "text")?)
            .map(|p| format!("appended, now revision {}.\n\n{}", p.revision, plan::describe(&p))),
        "plan_read" => {
            let p = plan::load(session_id);
            if p.body.trim().is_empty() {
                Ok("no plan yet for this conversation. Write one with plan_write.".to_string())
            } else {
                Ok(plan::describe(&p))
            }
        }

        "todo_write" => {
            let values = args.get("todos").and_then(Value::as_array).ok_or_else(|| "todos must be an array".to_string())?;
            todos::write(session_id, values).map(|list| todos::render(&list))
        }
        "todo_add" => {
            let values = args.get("todos").and_then(Value::as_array).ok_or_else(|| "todos must be an array".to_string())?;
            todos::add(session_id, values).map(|list| todos::render(&list))
        }
        "todo_update" => {
            let values = args.get("updates").and_then(Value::as_array).ok_or_else(|| "updates must be an array".to_string())?;
            todos::update(session_id, values).map(|update| format!("updated {} item(s).\n{}", update.changed, todos::render(&update.list)))
        }
        "todo_read" => {
            let list = todos::load(session_id);
            if list.items.is_empty() { Ok("no todo list yet. Write one with todo_write.".to_string()) }
            else { Ok(todos::render(&list)) }
        }

        // `wait` is not in this arm: it is a core tool, reachable by a
        // sub-agent, and the host serves it whether or not the session may
        // delegate. See the note beside its definition.
        "wait" => wait_for(&args),

        "spawn_agent" | "agent_status" | "agent_transcript" | "cancel_agent"
        | "agent_profiles" => {
            // The one-level rule, enforced a second time at the call. The
            // withheld tool definition is the cheap enforcement; this is the
            // one that holds when the model names a tool it saw earlier in the
            // conversation, or in a skill body.
            if !delegation_available() {
                return Err(
                    "delegation is not available in this session. A sub-agent cannot spawn \
                     sub-agents: do the work yourself, or report back to your parent that it \
                     needs splitting differently."
                        .to_string(),
                );
            }
            match name {
                "spawn_agent" => spawn_agent(&args),
                "agent_status" => Ok(format_children(&delegation::children(), now_ms())),
                "agent_transcript" => agent_transcript(&args),
                "cancel_agent" => delegation::cancel_child(&req_str(&args, "child_id")?)
                    .map(|row| format!("cancelled\n\n{}", format_child(&row, now_ms()))),
                _ => Ok(format_profiles(
                    &delegation::profiles(),
                    &delegation::limits(),
                )),
            }
        }

        // Recall. No `delegation_available` gate and no capability flag: these
        // only read, and a sub-agent needs them as much as a parent does.
        "conversation_list" => {
            let rows = transcripts::conversations(
                flag(&args, "include_archived"),
                flag(&args, "include_subagents"),
                args.get("limit").and_then(Value::as_u64).unwrap_or(0),
            )?;
            Ok(format_conversations(&rows, now_ms()))
        }
        "conversation_read" => {
            let id = req_str(&args, "session_id")?;
            let entries = transcripts::read(
                &id,
                args.get("from_seq").and_then(Value::as_u64).unwrap_or(0),
                args.get("limit").and_then(Value::as_u64).unwrap_or(0),
                args.get("max_chars").and_then(Value::as_u64).unwrap_or(0),
            )?;
            let header = transcripts::conversation(&id)
                .map(|c| format_conversation_header(&c, now_ms()))
                .unwrap_or_else(|_| format!("`{id}`"));
            Ok(format_transcript_entries(&header, &entries))
        }
        "conversation_grep" => {
            let report = transcripts::search(&transcripts::SearchQuery {
                pattern: req_str(&args, "pattern")?,
                session_id: opt_str(&args, "session_id"),
                include_archived: flag(&args, "include_archived"),
                include_subagents: flag(&args, "include_subagents"),
                include_tool_output: flag(&args, "include_tool_output"),
                max_results: args.get("max_results").and_then(Value::as_u64).unwrap_or(0),
                max_chars: args.get("max_chars").and_then(Value::as_u64).unwrap_or(0),
            })?;
            Ok(format_search_report(&report))
        }
        "subagent_list" => {
            // Defaulting to this conversation makes the common call argument-
            // free, and is why `session_id` is optional.
            let root = match opt_str(&args, "session_id") {
                s if s.is_empty() => session_id.to_string(),
                s => s,
            };
            let rows = transcripts::subagents(&root)?;
            if rows.is_empty() {
                return Ok(format!(
                    "no sub-agents under `{root}`. If you meant the children you spawned in \
                     this turn, agent_status is the tool for that."
                ));
            }
            Ok(format_conversations(&rows, now_ms()))
        }

        "exec" => {
            let command = req_str(&args, "command")?;
            let timeout = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(30_000) as u32;
            Ok(format_exec(sandbox::exec(session_id, &command, None, timeout)))
        }
        "write_file" => sandbox::write_file(
            session_id,
            &req_str(&args, "path")?,
            &req_str(&args, "contents")?,
        )
        .map(|_| "written".to_string()),
        "read_file" => sandbox::read_file(session_id, &req_str(&args, "path")?),

        "new_tool" => Ok(format_report(devkit::new_tool(
            &req_str(&args, "name")?,
            &req_str(&args, "description")?,
        ))),
        "write_code" => Ok(format_report(devkit::write_file(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "path")?,
            &req_str(&args, "contents")?,
        ))),
        "patch_code" => Ok(format_report(devkit::patch_file(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "path")?,
            &req_str(&args, "old_text")?,
            &req_str(&args, "new_text")?,
        ))),
        "add_dependency" => Ok(format_report(devkit::add_dependency(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &Dependency {
                name: req_str(&args, "name")?,
                version: req_str(&args, "version")?,
                features: args
                    .get("features")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
                default_features: args
                    .get("default_features")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            },
        ))),
        "remove_dependency" => Ok(format_report(devkit::remove_dependency(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "name")?,
        ))),
        "list_dependencies" => {
            let deps = devkit::list_dependencies(&parse_mod_target(&req_str(&args, "target")?)?)?;
            if deps.is_empty() {
                return Ok("no dependencies".to_string());
            }
            Ok(deps
                .iter()
                .map(format_dependency)
                .collect::<Vec<_>>()
                .join("\n"))
        }

        "read_code" => devkit::read_file(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "path")?,
        ),
        "list_code" => devkit::list_files(&parse_mod_target(&req_str(&args, "target")?)?)
            .map(|files| files.join("\n")),

        "branch_status" => branch::status().map(format_branch_state),
        "branch_log" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as u32;
            let entries = branch::log(limit);
            if entries.is_empty() {
                return Ok("no commits yet on this branch".to_string());
            }
            Ok(entries
                .iter()
                .map(|c| {
                    format!(
                        "{}  {}{}  {}",
                        &c.rev[..12.min(c.rev.len())],
                        if c.on_trunk { "[trunk] " } else { "" },
                        c.author,
                        c.subject
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "update_from_trunk" => branch::update_from_trunk().map(format_branch_state),
        "reset_branch" => branch::reset_to(&req_str(&args, "rev")?).map(format_branch_state),
        "complete_merge" => branch::complete_merge(
            args.get("message").and_then(Value::as_str),
        )
        .map(format_branch_state),
        "abort_merge" => branch::abort_merge().map(format_branch_state),

        "read_path" => hostfs::read_file_range(
            &req_str(&args, "path")?,
            args.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32,
            args.get("limit").and_then(Value::as_u64).unwrap_or(0) as u32,
        ),
        "edit_path" => hostfs::edit_file(
            &req_str(&args, "path")?,
            &req_str(&args, "old_text")?,
            &req_str(&args, "new_text")?,
            args.get("replace_all").and_then(Value::as_bool).unwrap_or(false),
        ),
        "search_files" => hostfs::search_files(
            &req_str(&args, "pattern")?,
            opt_arg(&args, "path").as_deref(),
            opt_arg(&args, "glob").as_deref(),
            &opt_str(&args, "mode"),
            args.get("max_results").and_then(Value::as_u64).unwrap_or(0) as u32,
        ),
        "find_files" => hostfs::find_files(
            &req_str(&args, "glob")?,
            opt_arg(&args, "path").as_deref(),
            args.get("max_results").and_then(Value::as_u64).unwrap_or(0) as u32,
        ),
        "write_path" => hostfs::write_file(
            &req_str(&args, "path")?,
            &req_str(&args, "contents")?,
        ),
        "list_path" => hostfs::list_dir(&req_str(&args, "path")?).map(format_listing),
        "delete_path" => hostfs::delete_path(
            &req_str(&args, "path")?,
            args.get("recursive").and_then(Value::as_bool).unwrap_or(false),
        ),

        "git_clone" => git_clone(&args),

        "terminal_open" => {
            let env: Vec<EnvVar> = args
                .get("env")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .map(|(key, value)| EnvVar {
                            key: key.clone(),
                            // A number or a bool in an env map is a slip, not a
                            // reason to fail: render it rather than dropping it.
                            value: match value {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default();
            terminal::open(&TerminalOpen {
                cwd: opt_arg(&args, "cwd"),
                name: opt_arg(&args, "name"),
                env,
                host: opt_arg(&args, "host"),
            })
        }
        "terminal_run" => terminal::run(
            &req_str(&args, "id")?,
            &req_str(&args, "command")?,
            args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
            args.get("background").and_then(Value::as_bool).unwrap_or(false),
        )
        .map(format_terminal),
        "terminal_read" => terminal::read(&req_str(&args, "id")?).map(|text| {
            if text.trim().is_empty() {
                "[nothing new]".to_string()
            } else {
                text
            }
        }),
        "terminal_send" => terminal::send(
            &req_str(&args, "id")?,
            // Deliberately not `req_str`: an empty string is a legitimate
            // payload, being how a bare newline is sent.
            args.get("text").and_then(Value::as_str).unwrap_or(""),
            args.get("submit").and_then(Value::as_bool).unwrap_or(true),
        ),
        "terminal_signal" => {
            terminal::signal(&req_str(&args, "id")?, &req_str(&args, "signal")?)
        }
        "terminal_close" => terminal::close(&req_str(&args, "id")?),
        "terminal_list" => {
            let sessions = terminal::sessions();
            if sessions.is_empty() {
                return Ok("no open sessions".to_string());
            }
            Ok(sessions
                .iter()
                .map(|s| {
                    let mut line = s.id.clone();
                    if !s.name.is_empty() {
                        line.push_str(&format!(" ({})", s.name));
                    }
                    // Where it runs comes before where it is: a path alone does
                    // not say which machine it is a path on.
                    line.push_str(&format!(
                        "  {}  {}  {} command(s)  {}",
                        if s.remote.is_empty() {
                            "local".to_string()
                        } else {
                            format!("ssh:{}", s.remote)
                        },
                        s.cwd,
                        s.commands,
                        if s.alive { "running" } else { "exited" }
                    ));
                    if !s.busy.is_empty() {
                        line.push_str(&format!("\n    busy in the background: {}", s.busy));
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }

        "ssh_host_list" => {
            let hosts = terminal::ssh_hosts()?;
            if hosts.is_empty() {
                return Ok("no ssh hosts defined. Add one with ssh_host_set, giving at least \
                           name and host."
                    .to_string());
            }
            Ok(hosts
                .iter()
                .map(format_ssh_host)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "ssh_host_get" => {
            let name = req_str(&args, "name")?;
            let hosts = terminal::ssh_hosts()?;
            match hosts.iter().find(|h| h.name == name) {
                Some(host) => Ok(format_ssh_host(host)),
                None => Err(format!(
                    "no ssh host named {name:?}. Defined: {}",
                    if hosts.is_empty() {
                        "none".to_string()
                    } else {
                        hosts
                            .iter()
                            .map(|h| h.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )),
            }
        }
        "ssh_host_set" => terminal::ssh_host_set(
            &SshHostInfo {
                name: req_str(&args, "name")?,
                host: opt_str(&args, "host"),
                port: args.get("port").and_then(Value::as_u64).unwrap_or(0) as u32,
                user: opt_str(&args, "user"),
                identity_file: opt_str(&args, "identity_file"),
                options: args
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                remote_cwd: opt_str(&args, "remote_cwd"),
                pty: args.get("pty").and_then(Value::as_bool).unwrap_or(false),
                description: opt_str(&args, "description"),
            },
            // Always a merge: editing one field of a host should not silently
            // clear the rest, and there is no plausible reason to want that.
            true,
        ),
        "ssh_host_remove" => terminal::ssh_host_remove(&req_str(&args, "name")?),
        "ssh_host_rename" => {
            terminal::ssh_host_rename(&req_str(&args, "name")?, &req_str(&args, "to")?)
        }

        "list_config" => {
            let entries = configuration::settings(args.get("prefix").and_then(Value::as_str));
            if entries.is_empty() {
                return Ok("no settings found".to_string());
            }
            Ok(entries
                .iter()
                .map(format_setting)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "read_config" => {
            let key = req_str(&args, "key")?;
            configuration::get(&key)
                .map(|e| format_setting(&e))
                .ok_or_else(|| format!("no setting named '{key}'"))
        }
        "set_config" => {
            configuration::set(&req_str(&args, "key")?, &req_str(&args, "value")?)
        }

        "restart_orchestrator" => control::restart(
            &req_str(&args, "reason")?,
            // Carrying on is the sensible default: a restart is usually a step
            // in the middle of doing something, not the end of it.
            args.get("resume").and_then(Value::as_bool).unwrap_or(true),
        ),

        // Anything else must be a hot-loaded tool component.
        other => tooling::invoke(other, session_id, args_json),
    }
}

// --- asking the user --------------------------------------------------------
//
// One call presents a whole form. See `ask_user` for why it does not block.

/// The largest number of questions one call may present.
///
/// A form long enough to scroll past is a form nobody finishes, and Discord
/// allows only five component rows per message, so this is both a UX limit and
/// a wire limit.
const MAX_QUESTIONS: usize = 5;

/// The tool's name, in one place because the turn loop matches on it to end
/// the turn after a question is asked. A literal in both spots could drift
/// apart, and the failure would be silent: the pause would stop happening.
pub const ASK_USER: &str = "ask_user";

/// The name of the tool that recovers a withheld tool group.
pub const TOOL_SEARCH: &str = "tool_search";

/// Ranks tool groups against a description and loads the ones that match.
///
/// Append-only by construction (see `groups::admit`): the model can widen its
/// own surface but never narrow it, so a tool it has already been shown cannot
/// disappear underneath it. The threshold here is looser than the routing one
/// because an explicit search is explicit evidence — the failure this guards
/// against is a capability the model cannot reach, which is worse than a few
/// hundred tokens of tool schema it does not use.
fn tool_search(session_id: &str, args: &Value) -> Result<String, String> {
    let query = opt_str(args, "query");
    let explicit: Vec<String> = args
        .get("load")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if !groups::grouping_enabled() {
        return Ok(format!(
            "Tool scoping is off in this deployment: every tool is already in your list. \
             The groups are:\n\n{}",
            groups::catalogue(&groups::all_ids())
        ));
    }

    let mut wanted = explicit.clone();

    if wanted.is_empty() && !query.trim().is_empty() {
        // Every group scoring above zero, plus the best one regardless. A search
        // that matched nothing but still names a real need should get the
        // closest thing rather than nothing at all.
        let ranked = groups::rank(&query);
        for (group, s) in ranked.iter() {
            if *s > 0.0 {
                wanted.push(group.id.to_string());
            }
        }
        if wanted.is_empty() {
            if let Some((best, _)) = ranked.first() {
                wanted.push(best.id.to_string());
            }
        }
    }

    if wanted.is_empty() {
        let active = groups::active(session_id);
        return Ok(format!(
            "Tool groups:\n\n{}\nCall this again with a query or `load` to add one.",
            groups::catalogue(&active)
        ));
    }

    let added = groups::admit(session_id, &wanted);
    let active = groups::active(session_id);

    if added.is_empty() {
        return Ok(format!(
            "Nothing to add — those groups are already loaded.\n\n{}",
            groups::catalogue(&active)
        ));
    }

    sys::log(
        LogLevel::Info,
        &format!("tool_search loaded groups: {}", added.join(", ")),
    );

    Ok(format!(
        "Loaded {} for the rest of this conversation; the tools appear in your list from your \
         next message onward.\n\n{}",
        added
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", "),
        groups::catalogue(&active)
    ))
}

/// Choices per question. Discord's select menu allows 25 options, and one is
/// spent on "something else", so 24 is the real ceiling.
const MAX_OPTIONS: usize = 24;

/// Presents questions to the user and returns immediately.
///
/// The tool does not wait for an answer, and cannot: a turn is a single pass
/// and the user's reply arrives as their next message, which starts the turn
/// after this one. So the work here is validation and normalisation — the tool
/// call event is what the surfaces render, and both the browser and Discord
/// read the normalised form out of `arguments_json`. Rejecting a malformed
/// question here rather than in a renderer means one error message instead of
/// two silently different behaviours.
fn ask_user(args: &Value) -> Result<String, String> {
    let raw = args
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing required argument 'questions' (an array)".to_string())?;

    if raw.is_empty() {
        return Err("'questions' was empty; ask at least one question".to_string());
    }
    if raw.len() > MAX_QUESTIONS {
        return Err(format!(
            "{} questions is too many to answer at once; ask at most {MAX_QUESTIONS} and follow \
             up next turn",
            raw.len()
        ));
    }

    let mut summary = Vec::new();
    for (index, item) in raw.iter().enumerate() {
        let position = index + 1;
        let question = item
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| format!("question {position} has no 'question' text"))?;

        let options: Vec<&str> = item
            .get("options")
            .and_then(Value::as_array)
            .map(|opts| {
                opts.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|o| !o.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // The kind is inferred when it is not stated, because a model that
        // supplies options plainly means a choice question, and refusing on a
        // missing field it did not need would be pedantry.
        let kind = match item.get("type").and_then(Value::as_str) {
            Some("choice") => "choice",
            Some("open") => "open",
            Some(other) => {
                return Err(format!(
                    "question {position} has type '{other}'; use 'choice' or 'open'"
                ))
            }
            None if options.is_empty() => "open",
            None => "choice",
        };

        if kind == "choice" {
            if options.is_empty() {
                return Err(format!(
                    "question {position} is multiple choice but lists no options; give some \
                     options, or use type 'open'"
                ));
            }
            if options.len() > MAX_OPTIONS {
                return Err(format!(
                    "question {position} has {} options; at most {MAX_OPTIONS} can be shown",
                    options.len()
                ));
            }
        }

        summary.push(match kind {
            "choice" => format!(
                "  {position}. {question}\n     options: {}",
                options.join(" / ")
            ),
            _ => format!("  {position}. {question}\n     open ended"),
        });
    }

    // Said back plainly, because this string is what the model reads next. It
    // no longer has to *ask* for the turn to end — the loop ends it as soon as
    // this call succeeds — but it still has to stop the model from restating
    // the questions in prose, since whatever it writes next is the last thing
    // the user reads before the form.
    Ok(format!(
        "Put {} question(s) to the user:\n{}\n\nEvery choice question also offers a free-text \
         answer, and every question can be skipped. This turn now ends automatically and the \
         answers arrive as the user's next message, so do not ask these again in your reply and \
         do not attempt further work: anything you plan to do with the answers belongs in the \
         next turn. A brief sign-off is fine.",
        raw.len(),
        summary.join("\n")
    ))
}

// --- git ---------------------------------------------------------------------

/// Wraps a value in single quotes so the shell takes it literally.
///
/// Every argument here comes from the model, and one of them is a path. Quoting
/// is what stops a repo name containing `;` from being a second command.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// `owner/repo`, from either that or a URL, rejecting anything else.
///
/// Validated rather than passed through: a bad name should be a clear error
/// here, not a confusing git failure three commands later.
fn parse_repo(raw: &str) -> Result<String, String> {
    let slug = raw
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("github.com/")
        .trim_start_matches("git@github.com:")
        .trim_end_matches(".git");

    let mut parts = slug.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!(
            "'{raw}' is not a repository; give 'owner/repo' or a GitHub URL"
        ));
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !ok(owner) || !ok(name) {
        return Err(format!(
            "'{raw}' is not a repository; give 'owner/repo' or a GitHub URL"
        ));
    }
    Ok(format!("{owner}/{name}"))
}

/// Clone a repository into a real working tree.
///
/// The token never reaches the command line or `.git/config`: the credential
/// script's `env` output installs a `url.insteadOf` rewrite, so git substitutes
/// the credential itself and the stored remote stays tokenless. That matters
/// because an installation token expires in an hour — a copy written to disk
/// would be both a leaked secret and a stale one.
fn git_clone(args: &Value) -> Result<String, String> {
    let repo = parse_repo(&req_str(args, "repo")?)?;
    let name = repo.split('/').nth(1).unwrap_or("repo").to_string();

    let dir = match opt_arg(args, "dir") {
        Some(dir) => {
            let dir = dir.trim().trim_start_matches("./").to_string();
            if dir.starts_with('/') || dir.split('/').any(|p| p == "..") {
                return Err(
                    "dir must be a relative path inside your checkout, with no '..'".to_string(),
                );
            }
            dir
        }
        None => format!("{CLONE_ROOT}/{name}"),
    };

    let depth = match args.get("depth").and_then(Value::as_u64) {
        Some(n) if n > 0 => format!("--depth {n} "),
        _ => String::new(),
    };
    let clone = format!(
        "git clone {depth}{} {}",
        shell_quote(&format!("https://github.com/{repo}.git")),
        shell_quote(&dir),
    );

    // One shell, because every step after the first depends on the environment
    // `eval` sets up, and `set -e` so a failed clone does not go on to report a
    // commit identity for a tree that does not exist.
    //
    // The token is captured before the eval rather than relying on `set -e` to
    // catch a failure: `eval "$(...)"` reports the status of `eval`, which
    // succeeds on the empty string a failed script leaves behind. Without this
    // check, a missing credential reached git as "could not read Username for
    // 'https://github.com'", which sends the reader looking at ssh keys instead
    // of at [tools.git].
    let mut script = format!(
        "set -e\n\
         __creds=$(scripts/github-app-git.sh env) || {{ echo \"$__creds\"; exit 1; }}\n\
         eval \"$__creds\"\n\
         mkdir -p \"$(dirname {dir})\"\n\
         {clone}\n",
        dir = shell_quote(&dir),
    );
    if let Some(reference) = opt_arg(args, "ref") {
        script.push_str(&format!(
            "git -C {} checkout {}\n",
            shell_quote(&dir),
            shell_quote(&reference)
        ));
    }
    // The identity has to be set per clone: GIT_AUTHOR_* only lasts as long as
    // the session that ran `env`, and a later terminal would commit as nobody.
    script.push_str(&format!(
        "git -C {dir} config user.name \"$GIT_AUTHOR_NAME\"\n\
         git -C {dir} config user.email \"$GIT_AUTHOR_EMAIL\"\n\
         git -C {dir} log --oneline -1\n\
         git -C {dir} remote -v | head -1\n",
        dir = shell_quote(&dir),
    ));

    // A local session, labelled so it is recognisable in a listing if the clone
    // is slow enough that someone looks.
    let session = terminal::open(&TerminalOpen {
        cwd: None,
        name: Some(format!("git_clone {repo}")),
        env: Vec::new(),
        host: None,
    })?;
    // Cloning is network-bound and a large repository is slow, so this gets a
    // longer leash than the terminal default. The session is closed either way.
    // Not backgrounded: the whole point here is to wait for the clone.
    let result = terminal::run(&session, &script, 600_000, false);
    let _ = terminal::close(&session);

    let output = format_terminal(result?);
    Ok(format!(
        "cloned {repo} into {dir}\n\n{output}\n\n\
         The remote is tokenless and the [bot] identity is set on this clone. Before pushing, \
         run: eval \"$(scripts/github-app-git.sh env)\" in the terminal session you push from."
    ))
}

// --- memory tools -----------------------------------------------------------

const INDEX_KEY: &str = "__memory_index";

fn remember(session_id: &str, args: &Value) -> Result<String, String> {
    let key = req_str(args, "key")?;
    let value = req_str(args, "value")?;
    if key.starts_with("__") {
        return Err("keys beginning with '__' are reserved".to_string());
    }

    sys::kv_put(session_id, &key, &value);

    // Maintain an index so `recall` can enumerate; the KV store itself has no
    // listing operation.
    let mut keys = memory_keys(session_id);
    if !keys.iter().any(|k| k == &key) {
        keys.push(key.clone());
        keys.sort();
        sys::kv_put(session_id, INDEX_KEY, &keys.join("\n"));
    }

    sys::log(LogLevel::Debug, &format!("remembered '{key}'"));
    Ok(format!("remembered '{key}'"))
}

fn recall(session_id: &str, args: &Value) -> Result<String, String> {
    match args.get("key").and_then(Value::as_str) {
        Some(key) => sys::kv_get(session_id, key)
            .ok_or_else(|| format!("nothing remembered under '{key}'")),
        None => {
            let keys = memory_keys(session_id);
            if keys.is_empty() {
                return Ok("nothing remembered yet".to_string());
            }
            Ok(keys
                .iter()
                .map(|k| {
                    let value = sys::kv_get(session_id, k).unwrap_or_default();
                    format!("{k}: {value}")
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

fn memory_keys(session_id: &str) -> Vec<String> {
    sys::kv_get(session_id, INDEX_KEY)
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// --- delegation -------------------------------------------------------------

/// Starts a sub-agent.
///
/// The brief is checked for length here rather than only host-side, because the
/// failure a one-line brief causes is silent: the child does plausible work
/// against the wrong objective and the cost is only visible at the end. A
/// refusal that says what is missing is cheaper than a wasted conversation.
/// The shortest brief worth sending.
///
/// A one-line brief is the commonest way delegation fails: the child cannot see
/// the conversation, so "look into the auth bug" gives it nothing, and it burns
/// a whole session working out what was meant. The number is a floor rather
/// than a judgement of quality — it is enough to catch the reflexive one-liner.
const MIN_BRIEF: usize = 40;
// Measured in characters, not bytes, so the floor is the same in every script.

/// Whether a brief is long enough to act on, as a message when it is not.
/// Split from `spawn_agent` so it is testable off-wasm.
fn check_brief(task: &str) -> Result<(), String> {
    if task.trim().chars().count() >= MIN_BRIEF {
        return Ok(());
    }
    Err(
        "that brief is too short to act on. The sub-agent cannot see this conversation, so \
         state the objective, what to return and in what shape, where to look, and the \
         boundaries."
            .to_string(),
    )
}

fn spawn_agent(args: &Value) -> Result<String, String> {
    let label = req_str(args, "label")?;
    let task = req_str(args, "task")?;
    check_brief(&task)?;

    let row = delegation::spawn(&delegation::SpawnRequest {
        label,
        task,
        profile: opt_str(args, "profile"),
        model: opt_str(args, "model"),
        mode: opt_str(args, "mode"),
    })?;

    Ok(format!(
        "started sub-agent `{}` ({})\n\nIt runs while you carry on. Call `wait` when you need \
         its answer; do not poll.",
        row.label, row.id
    ))
}

/// Blocks on a predicate.
///
/// Both arguments are optional and the default is chosen from the situation:
/// with children to watch, "all" is what a parent nearly always means, and
/// guessing right saves a refused call and a retry. With none, the only
/// sensible reading is a plain sleep.
fn wait_for(args: &Value) -> Result<String, String> {
    let children: Vec<String> = args
        .get("children")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let limits = delegation::limits();
    let mine = delegation::children();
    let watching = !children.is_empty() || mine.iter().any(|c| c.state == "running");

    let (until, timeout) = wait_plan(args, watching, limits.max_wait_secs)?;

    let out = delegation::wait(&until, &children, timeout)?;

    let mut text = if out.timed_out {
        // A timeout is the deadline expiring, not a verdict on the children.
        // Said plainly here because the alternative reading — "they are stuck"
        // — is the one that gets a working fan-out cancelled wholesale.
        format!(
            "waited {timeout}s and timed out — {}\n\nThis says the deadline passed, not that \
             anything is wrong. Check the per-child progress below before deciding: a child \
             whose log and cost are still moving is working, however long it has been.",
            out.reason
        )
    } else {
        format!("wait ended: {}", out.reason)
    };
    if out.children.is_empty() {
        text.push_str("\n\nNo sub-agents.");
    } else {
        text.push_str("\n\n");
        text.push_str(&format_children(&out.children, now_ms()));
    }
    Ok(text)
}

/// The part of `wait_for` that is a decision rather than a call: which
/// predicate, and for how long. Split out so it can be tested on the host,
/// where the delegation imports do not exist.
///
/// `watching` is whether there is anything to wait *for* — either children were
/// named or some child of this session is still running.
fn wait_plan(args: &Value, watching: bool, max_wait_secs: u64) -> Result<(String, u64), String> {
    let until = match opt_arg(args, "until") {
        Some(u) => u,
        None if watching => "all".to_string(),
        None => "time".to_string(),
    };

    let asked = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if asked == 0 && until == "time" {
        return Err("a 'time' wait needs timeout_secs — it has nothing else to end it.".to_string());
    }

    // A wait with no deadline is indistinguishable from a hung turn, so one is
    // always supplied. The host clamps it too; this just avoids asking for
    // something that will be silently reduced.
    let timeout = if asked == 0 {
        max_wait_secs
    } else {
        asked.min(max_wait_secs)
    };
    Ok((until, timeout))
}

fn agent_transcript(args: &Value) -> Result<String, String> {
    let events = delegation::child_transcript(
        &req_str(args, "child_id")?,
        args.get("from_seq").and_then(Value::as_u64).unwrap_or(0),
    )?;
    if events.is_empty() {
        return Ok("no events".to_string());
    }
    Ok(format_transcript(&events))
}

/// A child's log, flattened to something readable.
///
/// Only the arms that say what the child *did* are rendered: messages, tool
/// calls and their outcomes, notes and incidents. Stream deltas and turn
/// bookkeeping are dropped, and tool output is truncated hard — this is read to
/// diagnose a child, and pulling its whole transcript into the parent's context
/// would undo the reason for delegating in the first place.
fn format_transcript(events: &[EventRecord]) -> String {
    /// Per-item ceiling. Generous enough for an error message, mean enough that
    /// a hundred of them still fit.
    const SNIP: usize = 600;

    fn snip(text: &str) -> String {
        let trimmed = text.trim();
        match trimmed.char_indices().nth(SNIP) {
            None => trimmed.to_string(),
            Some((cut, _)) => format!("{}… [{} chars]", &trimmed[..cut], trimmed.len()),
        }
    }

    let mut out: Vec<String> = Vec::new();
    for record in events {
        let line = match &record.event {
            SessionEvent::UserMessage(msg) => format!("**brief:** {}", snip(&msg.text)),
            SessionEvent::AssistantMessage(msg) => {
                let mut parts: Vec<String> = Vec::new();
                if !msg.content.trim().is_empty() {
                    parts.push(snip(&msg.content));
                }
                for call in &msg.tool_calls {
                    parts.push(format!("→ {}({})", call.name, snip(&call.arguments_json)));
                }
                if parts.is_empty() {
                    continue;
                }
                parts.join("\n")
            }
            SessionEvent::ToolResult(out) => {
                let tag = if out.ok { "ok" } else { "failed" };
                format!("← {tag}: {}", snip(&out.content))
            }
            SessionEvent::SystemNote(text) => format!("_note:_ {}", snip(text)),
            SessionEvent::Incident(text) => format!("**incident:** {}", snip(text)),
            SessionEvent::TurnFinished(stats) => {
                format!(
                    "_turn ended ({}), {} iterations, ${:.4}_",
                    stats.stopped_by, stats.iterations, stats.cost_usd
                )
            }
            // Deltas, turn starts, compaction and branch bookkeeping say
            // nothing about the work.
            _ => continue,
        };
        out.push(format!("[{}] {line}", record.seq));
    }

    if out.is_empty() {
        return "nothing but bookkeeping in that range".to_string();
    }
    out.join("\n\n")
}

// --- rendering transcripts for the model ------------------------------------
//
// These land in a tool result, so they are prose to be skimmed rather than a
// structure to be parsed. Two things they must always do: carry the session id
// and seq for every line, because those are what the next call needs as
// arguments, and say plainly when an answer is partial. A silently truncated
// recall result reads as a complete one and gets believed.

/// How long ago, in the coarsest unit that is still informative.
///
/// Relative rather than absolute, because "yesterday" is what a reader wants
/// from a conversation list, and an epoch millisecond timestamp is not
/// something either of us can read at a glance.
///
/// `now` is passed in rather than read from `sys::now_ms()` here, for the same
/// reason `wait_plan` is split from `wait_for`: the host imports do not exist on
/// the host test target, so a formatter that calls one cannot be tested at all —
/// it traps with `entered unreachable code`. The clock read happens once, at the
/// edge, in [`now_ms`].
fn ago(ts_ms: u64, now: u64) -> String {
    // A timestamp from the future means the clocks disagree, which is not worth
    // a branch of its own.
    if ts_ms == 0 || ts_ms >= now {
        return "just now".to_string();
    }
    let secs = (now - ts_ms) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// The clock, read once per rendered result.
fn now_ms() -> u64 {
    sys::now_ms()
}

/// One conversation as a heading line: what it is, and the id to pass next.
fn format_conversation_header(c: &transcripts::ConversationSummary, now: u64) -> String {
    let title = if c.is_subagent && !c.label.is_empty() {
        // A sub-agent's own title is auto-derived from its brief and so repeats
        // the task; the label is what the parent actually called it.
        format!("{} (sub-agent)", c.label)
    } else if c.title.trim().is_empty() {
        "untitled".to_string()
    } else {
        c.title.clone()
    };

    let mut facts: Vec<String> = vec![ago(c.updated_ms, now)];
    if !c.state.is_empty() {
        facts.push(c.state.clone());
    }
    if !c.mode.is_empty() && c.mode != DEFAULT_MODE {
        facts.push(format!("{} mode", c.mode));
    }
    facts.push(format!("{} events", c.event_count));
    if c.archived {
        facts.push("archived".to_string());
    }
    format!("### {title}\n`{}` · {}", c.id, facts.join(" · "))
}

fn format_conversations(rows: &[transcripts::ConversationSummary], now: u64) -> String {
    if rows.is_empty() {
        return "no conversations match.".to_string();
    }
    let mut out = vec![format!(
        "{} conversation(s), most recently active first:",
        rows.len()
    )];
    for c in rows {
        let mut block = format_conversation_header(c, now);
        // The brief for a sub-agent, the last thing said for a conversation:
        // whichever is more use in deciding whether to open it.
        let gist = if c.is_subagent && !c.task.trim().is_empty() {
            &c.task
        } else {
            &c.preview
        };
        if !gist.trim().is_empty() {
            block.push_str(&format!("\n{}", one_line(gist, 160)));
        }
        out.push(block);
    }
    out.join("\n\n")
}

/// Collapses text to a single clipped line, for a preview.
fn one_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    format!("{}…", flat.chars().take(max_chars).collect::<String>())
}

/// Marks what kind of line this is, compactly enough to sit in front of every
/// entry without becoming most of the output.
fn kind_marker(kind: &str) -> &'static str {
    match kind {
        "user" => "user:",
        "assistant" => "said:",
        "tool-call" => "→",
        "tool-result" => "←",
        "tool-failed" => "← FAILED",
        "note" => "note:",
        "nudge" => "nudge:",
        "incident" => "INCIDENT:",
        "modification" => "changed:",
        "branch-op" => "branch:",
        "turn-finished" => "turn:",
        "compacted" => "summary:",
        // A kind the host added and this build has not learned yet. Showing it
        // raw beats dropping the line: the text is still the answer.
        other => {
            debug_assert!(false, "unknown transcript kind {other}");
            "·"
        }
    }
}

fn format_transcript_entries(header: &str, entries: &[transcripts::TranscriptEntry]) -> String {
    if entries.is_empty() {
        return format!(
            "{header}\n\nNothing to show in that range — either the conversation is empty or \
             `from_seq` is past its end."
        );
    }
    let mut out = vec![header.to_string()];
    for e in entries {
        let mut line = format!("[{}] {} {}", e.seq, kind_marker(&e.kind), e.text);
        if e.elided > 0 {
            // Say what was cut, so a clipped line is never quoted back as if it
            // were the whole record.
            line.push_str(&format!(" …[{} more chars]", e.elided));
        }
        out.push(line);
    }
    let last = entries.last().map(|e| e.seq).unwrap_or(0);
    out.push(format!(
        "_{} entries, through seq {last}. Page on with from_seq={last}._",
        entries.len()
    ));
    out.join("\n\n")
}

fn format_search_report(report: &transcripts::SearchReport) -> String {
    if report.hits.is_empty() {
        let mut msg = format!(
            "no matches in {} conversation(s) searched.",
            report.scanned_conversations
        );
        if report.incomplete {
            msg.push_str(
                " The scan did not reach every conversation, so try a narrower pattern or name \
                 a session_id.",
            );
        } else {
            msg.push_str(
                " Try a looser pattern, `(?i)` for case-insensitivity, or \
                 include_tool_output=true if what you want was in a tool result.",
            );
        }
        return msg;
    }

    let mut out = vec![format!(
        "{} match(es) in {} conversation(s), from {} searched:",
        report.total_matches, report.matched_conversations, report.scanned_conversations
    )];

    // Grouped by conversation, because the useful next action is "open that
    // one", and a flat list makes the reader do the grouping themselves.
    let mut current = String::new();
    for hit in &report.hits {
        if hit.session_id != current {
            current = hit.session_id.clone();
            let who = if hit.is_subagent && !hit.label.is_empty() {
                format!("{} (sub-agent)", hit.label)
            } else if hit.title.trim().is_empty() {
                "untitled".to_string()
            } else {
                hit.title.clone()
            };
            out.push(format!("**{who}** · `{}`", hit.session_id));
        }
        out.push(format!(
            "  [{}] {} {}",
            hit.seq,
            kind_marker(&hit.kind),
            one_line(&hit.text, 300)
        ));
    }

    if report.capped {
        out.push(format!(
            "_Stopped at the first {} hits of {}. Narrow the pattern, or name a session_id._",
            report.hits.len(),
            report.total_matches
        ));
    } else if report.incomplete {
        out.push(
            "_A scan bound was reached before every conversation was read; the oldest are \
             missing. Narrow the pattern to be sure._"
                .to_string(),
        );
    }
    out.push("_Read any of these with conversation_read, passing the id above._".to_string());
    out.join("\n")
}

// --- formatting -------------------------------------------------------------

/// One sub-agent, as prose the model reads.
///
/// The answer is included in full whenever there is one: the point of
/// delegation is that the answer arrives without the work behind it, so making
/// the parent take a second call to read it would defeat the mechanism.
fn format_child(row: &delegation::SubagentInfo, now: u64) -> String {
    let mut out = format!("### {} — {}\n`{}`\n", row.label, row.state, row.id);

    let mut facts: Vec<String> = Vec::new();
    if !row.profile.is_empty() {
        facts.push(format!("profile {}", row.profile));
    }
    if !row.model.is_empty() {
        facts.push(row.model.clone());
    }
    if !row.mode.is_empty() && row.mode != DEFAULT_MODE {
        facts.push(format!("{} mode", row.mode));
    }
    // Elapsed for a finished child, and — the point of the live fields — for
    // one still going. Without it a running child carried no clock at all, so
    // thirty seconds and thirty minutes read the same.
    if let Some(elapsed) = elapsed_secs(row, now) {
        facts.push(format!("{elapsed}s"));
    }
    if row.cost_usd > 0.0 {
        facts.push(format!("${:.4}", row.cost_usd));
    }
    if !facts.is_empty() {
        out.push_str(&facts.join(" · "));
        out.push('\n');
    }

    if !row.detail.is_empty() {
        out.push_str(&format!("\n{}\n", row.detail));
    }
    if !row.answer.is_empty() {
        out.push_str(&format!("\n{}\n", row.answer));
    } else if row.state == "running" {
        out.push_str(&format!("\n{}\n", running_progress(row, now)));
    }
    out
}

/// How long a child has been going, finished or not.
///
/// `None` only when the row carries no usable clock at all, which is what a
/// freshly spawned child looks like for its first instant.
fn elapsed_secs(row: &delegation::SubagentInfo, now: u64) -> Option<u64> {
    let end = if row.finished_ms > row.created_ms {
        row.finished_ms
    } else if now > row.created_ms {
        now
    } else {
        return None;
    };
    Some((end - row.created_ms).div_ceil(1000))
}

/// What a running child is doing, in the terms that separate busy from stuck.
///
/// "Still working" was all this used to say, which is true of a child mid-tool
/// and equally true of one whose worker died half an hour ago. A parent that
/// cannot tell those apart has one move available to it — cancel — and will
/// eventually use it on work that was going fine. So: how much log the child
/// has written, and how long since it last wrote any.
fn running_progress(row: &delegation::SubagentInfo, now: u64) -> String {
    if row.events == 0 && row.activity_ms == 0 {
        // No live read got through. Say so rather than implying silence.
        return "Still working (no progress reading available).".to_string();
    }
    let last = ago(row.activity_ms, now);
    let stalled = row.activity_ms > 0
        && now > row.activity_ms
        && (now - row.activity_ms) >= STALE_CHILD_MS;
    let mut line = format!(
        "Still working — {} log entries, last activity {last}.",
        row.events
    );
    if stalled {
        line.push_str(
            " Nothing for a while: check `agent_transcript` before assuming it is wedged, \
             because a single long tool call looks exactly like this.",
        );
    }
    line
}

/// How quiet a running child has to go before its listing says so.
///
/// Generously long on purpose. One build, one large search or one slow
/// completion can hold a child silent for minutes without anything being
/// wrong, and a warning that fires on healthy work teaches a parent to ignore
/// it — which is the same failure as not having one.
const STALE_CHILD_MS: u64 = 10 * 60 * 1000;

fn format_children(rows: &[delegation::SubagentInfo], now: u64) -> String {
    if rows.is_empty() {
        return "You have not spawned any sub-agents.".to_string();
    }
    let running = rows.iter().filter(|r| r.state == "running").count();
    let failed = rows
        .iter()
        .filter(|r| r.state == "failed" || r.state == "cancelled")
        .count();
    let total: f64 = rows.iter().map(|r| r.cost_usd).sum();

    let mut head = format!("{} sub-agent(s)", rows.len());
    if running > 0 {
        head.push_str(&format!(", {running} running"));
    }
    if failed > 0 {
        head.push_str(&format!(", {failed} not completed"));
    }
    if total > 0.0 {
        head.push_str(&format!(", ${total:.4} so far"));
    }

    let body: Vec<String> = rows.iter().map(|r| format_child(r, now)).collect();
    format!("{head}\n\n{}", body.join("\n"))
}

fn format_profiles(
    profiles: &[delegation::AgentProfileInfo],
    limits: &delegation::DelegationLimits,
) -> String {
    let mut out = String::new();
    if profiles.is_empty() {
        out.push_str(
            "No profiles are configured, so a sub-agent runs with this conversation's own \
             model and the default mode. You can still override either on the spawn.\n",
        );
    } else {
        out.push_str("Profiles:\n");
        for p in profiles {
            out.push_str(&format!("\n- **{}** — {}", p.id, p.label));
            let mut how: Vec<String> = Vec::new();
            if !p.model.is_empty() {
                how.push(p.model.clone());
            }
            if !p.mode.is_empty() {
                how.push(format!("{} mode", p.mode));
            }
            if !how.is_empty() {
                out.push_str(&format!(" ({})", how.join(", ")));
            }
            if !p.description.is_empty() {
                out.push_str(&format!("\n  {}", p.description));
            }
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "\nLimits: {} running at once, waits capped at {}s, answers clamped to {} bytes.\n",
        limits.max_children, limits.max_wait_secs, limits.max_result_bytes
    ));
    out
}

/// An optional string argument, absent becoming empty.
///
/// The skills interface takes an empty string to mean "not given" for `file`
/// and `id`, so this collapses the two cases the model can produce - the key
/// missing, or the key present and null - into the one the host expects.
fn opt_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// An optional boolean argument, absent meaning false.
///
/// Every flag on this surface is written so that false is the conservative
/// reading — narrower search, fewer conversations, less noise — so a model that
/// omits one gets the safe behaviour rather than the expensive one.
fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An optional string argument where absent and blank mean the same thing.
///
/// The filesystem search takes `option<string>` rather than an empty string,
/// because "no path given" and "the path is the empty string" are different
/// answers there - the first means the project root, the second is an error.
fn opt_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

// --- rendering skills for the model ----------------------------------------
//
// These go into a tool result, which is prose the model reads rather than a
// structure it parses, so they favour being skimmable over being complete.

fn format_skill_body(body: skills::SkillBody) -> String {
    let mut out = String::new();

    // A well-formed body opens with its own `# Title`, so synthesising another
    // one here printed the heading twice. When the body brings a title, this
    // contributes only the locator; when it does not, it supplies both.
    let body_has_title = body
        .content
        .lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim_start().starts_with("# "));

    if body.file.is_empty() {
        if body_has_title {
            out.push_str(&format!("`{}`\n", body.id));
        } else {
            out.push_str(&format!("# {} ({})\n", body.name, body.id));
        }
    } else {
        out.push_str(&format!("# {} — {}\n", body.id, body.file));
    }

    if !body.children.is_empty() {
        out.push_str(&format!(
            "\nNested skills: {}\nFetch one by its full id.\n",
            body.children.join(", ")
        ));
    }
    if !body.resources.is_empty() {
        out.push_str(&format!(
            "\nBundled files: {}\nFetch one with the `file` argument.\n",
            body.resources.join(", ")
        ));
    }

    out.push('\n');
    out.push_str(&body.content);

    // Say so explicitly: a body that stops mid-sentence otherwise reads as the
    // whole of a badly written skill.
    if body.truncated {
        let shown = body.offset as usize + body.content.chars().count();
        out.push_str(&format!(
            "\n\n[truncated at {shown} of {} characters; fetch again with offset={shown} for the rest]",
            body.total
        ));
    }

    out
}

fn format_skill_cards(cards: &[skills::SkillCard]) -> String {
    if cards.is_empty() {
        return "no skills matched".to_string();
    }

    let mut out = format!("{} skill(s), best first:\n", cards.len());
    for card in cards {
        out.push_str(&format!("\n`{}` ({:.2})", card.id, card.score));
        if card.universal {
            out.push_str(" [universal]");
        }
        if card.status == "retired" {
            out.push_str(" [retired]");
        }
        out.push_str(&format!("\n  {}", card.brief));
        if card.status == "retired" && !card.superseded_by.is_empty() {
            out.push_str(&format!("\n  Superseded by `{}`.", card.superseded_by));
        }
        if !card.when_to_use.trim().is_empty() {
            out.push_str(&format!("\n  Use when: {}", card.when_to_use.trim()));
        }
        if !card.children.is_empty() {
            out.push_str(&format!("\n  Nested: {}", card.children.join(", ")));
        }
        if !card.related.is_empty() {
            out.push_str(&format!("\n  Related: {}", card.related.join(", ")));
        }
        if !card.resources.is_empty() {
            out.push_str(&format!("\n  Files: {}", card.resources.join(", ")));
        }
        out.push('\n');
    }
    out.push_str("\nFetch one with `skill_fetch` to read its instructions.");
    out
}

fn format_skill_write(w: skills::SkillWrite) -> String {
    let verb = if w.created { "created" } else { "updated" };
    let mut out = format!("{verb} `{}` at {}", w.id, w.path);
    if w.diagnostics.is_empty() {
        out.push_str("\nlint clean");
    } else {
        out.push('\n');
        out.push_str(&format_skill_diagnostics(&w.diagnostics));
    }
    out
}

fn format_skill_diagnostics(diags: &[skills::SkillDiagnostic]) -> String {
    let mut out = format!("{} diagnostic(s):", diags.len());
    for d in diags {
        out.push_str(&format!("\n  [{}] {}: {}", d.severity, d.id, d.message));
    }
    out
}

fn req_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument '{key}'"))
}

fn parse_mod_target(raw: &str) -> Result<ModTarget, String> {
    match raw {
        "self" | "agent" => Ok(ModTarget::AgentSelf),
        other => match other.split_once(':') {
            Some(("tool", name)) => Ok(ModTarget::Tool(name.to_string())),
            Some(("gateway", name)) => Ok(ModTarget::Gateway(name.to_string())),
            _ => Err(format!(
                "unknown target '{other}'; use 'self', 'tool:<name>', or 'gateway:<name>'"
            )),
        },
    }
}

/// Renders branch state the way the model needs to read it: position first,
/// then anything demanding action.
fn format_branch_state(state: branch::BranchState) -> String {
    let mut out = format!(
        "branch {} at {} — {} ahead of trunk, {} behind (trunk at {})\nstate: {}",
        state.branch,
        &state.head_rev[..12.min(state.head_rev.len())],
        state.ahead,
        state.behind,
        &state.trunk_rev[..12.min(state.trunk_rev.len())],
        state.state,
    );
    if !state.conflicts.is_empty() {
        out.push_str(&format!(
            "\nunresolved conflicts in:\n  {}",
            state.conflicts.join("\n  ")
        ));
        out.push_str(
            "\nResolve the conflict markers in these files with the editing tools, then call \
             complete_merge — or abort_merge to give up.",
        );
    }
    if !state.detail.is_empty() {
        out.push_str(&format!("\n{}", state.detail));
    }
    out
}

/// Renders a build result the way the model needs to read it: verdict first,
/// then the compiler's own words.
fn format_report(report: CompileReport) -> String {
    let mut out = String::new();
    if report.success {
        out.push_str(&format!(
            "BUILD OK — {} r{} in {:.1}s",
            report.aspect,
            report
                .revision
                .map(|r| r.to_string())
                .unwrap_or_else(|| "?".into()),
            report.duration_ms as f64 / 1000.0
        ));
        if report.pending_swap {
            out.push_str("\nThe new version takes effect when this turn ends.");
        }
    } else {
        out.push_str(&format!(
            "BUILD FAILED — {} (unchanged, still running the previous revision)",
            report.aspect
        ));
    }

    if !report.detail.is_empty() {
        out.push_str(&format!("\n{}", report.detail));
    }
    if !report.stderr.is_empty() {
        out.push_str(&format!("\n\n{}", report.stderr));
    }
    out
}

/// One setting on a line, marked where it cannot be changed.
fn format_setting(entry: &ConfigEntry) -> String {
    let mut line = format!("{} = {}", entry.key, entry.value);
    if !entry.editable {
        line.push_str("   [read-only]");
    } else if !entry.live {
        line.push_str("   [needs restart]");
    }
    line
}

/// One dependency on a line, in the shape it takes in the manifest.
fn format_dependency(dep: &Dependency) -> String {
    let mut line = format!("{} = \"{}\"", dep.name, dep.version);
    if !dep.features.is_empty() {
        line.push_str(&format!("   features: {}", dep.features.join(", ")));
    }
    if !dep.default_features {
        line.push_str("   [no default features]");
    }
    line
}

/// A directory listing, sized and marked so it reads at a glance.
fn format_listing(entries: Vec<FsEntry>) -> String {
    if entries.is_empty() {
        return "[empty directory]".to_string();
    }
    entries
        .iter()
        .map(|e| {
            if e.is_dir {
                format!("{}/", e.name)
            } else {
                format!("{}  ({})", e.name, human_size(e.size))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_terminal(result: TerminalOutput) -> String {
    let mut out = String::new();
    if result.truncated {
        out.push_str("[earlier output trimmed]\n");
    }
    if result.output.trim().is_empty() {
        out.push_str("[no output]");
    } else {
        out.push_str(&result.output);
    }
    // A non-zero status is the thing most easily missed in a wall of output, so
    // it goes last where it will be read, and names the host when there is one.
    if let Some(code) = result.exit_code.filter(|c| *c != 0) {
        out.push_str(&format!("\n\n[exit status {code}"));
        if !result.host.is_empty() {
            out.push_str(&format!(" on {}", result.host));
        }
        out.push(']');
    }
    // A backgrounded command is also "timed out" internally, but it said so
    // itself and in its own words; repeating it as a timeout would be wrong.
    // Anchored at the start rather than searched for: the note is always the
    // first thing in a background result, and a `contains` would also match a
    // command whose own output happened to quote the phrase.
    if result.timed_out && !result.output.starts_with("[started in the background") {
        out.push_str(
            "\n\n[still running: the timeout elapsed. Use terminal_read to collect the rest, or \
             terminal_signal to interrupt it.]",
        );
    }
    out
}

fn format_ssh_host(host: &SshHostInfo) -> String {
    let mut parts = vec![if host.user.is_empty() {
        host.host.clone()
    } else {
        format!("{}@{}", host.user, host.host)
    }];
    if host.port != 0 {
        parts.push(format!("port {}", host.port));
    }
    if !host.identity_file.is_empty() {
        parts.push(format!("key {}", host.identity_file));
    }
    if !host.remote_cwd.is_empty() {
        parts.push(format!("cwd {}", host.remote_cwd));
    }
    if host.pty {
        parts.push("pty".to_string());
    }
    for option in &host.options {
        parts.push(option.clone());
    }
    let mut line = format!("{}  {}", host.name, parts.join("  ·  "));
    if !host.description.is_empty() {
        line.push_str(&format!("\n    {}", host.description));
    }
    line
}

fn format_exec(result: ExecResult) -> String {
    let mut out = String::new();
    if result.timed_out {
        out.push_str("[timed out]\n");
    }
    if !result.stdout.is_empty() {
        out.push_str(&result.stdout);
    }
    if !result.stderr.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[stderr]\n{}", result.stderr));
    }
    if result.exit_code != 0 {
        out.push_str(&format!("\n[exit {}]", result.exit_code));
    }
    if result.truncated {
        out.push_str("\n[output truncated]");
    }
    if out.is_empty() {
        out.push_str("[no output]");
    }
    out
}

/// Tests for the recall tools' rendering.
///
/// The formatters are the whole of the agent-side logic here — the searching
/// itself is the host's, and is tested in `crates/thetis/src/transcripts.rs`.
/// What these pin is the part a model acts on: that an id it needs for the next
/// call is present, and that a partial answer says so.
#[cfg(test)]
mod recall_tests {
    use super::{
        ago, flag, format_conversations, format_search_report, format_transcript_entries, one_line,
    };
    use crate::thetis::grip::transcripts;
    use serde_json::json;

    /// A fixed "now", so the rendered ages are the same on every run. The
    /// formatters take the clock as an argument precisely so this is possible.
    /// Large enough that subtracting a few days from it stays positive.
    const NOW: u64 = 1_700_000_000_000;

    fn conversation(id: &str, title: &str) -> transcripts::ConversationSummary {
        transcripts::ConversationSummary {
            id: id.to_string(),
            title: title.to_string(),
            mode: "agent".into(),
            model: String::new(),
            preview: "the last thing said".into(),
            created_ms: 1_000,
            updated_ms: 1_000,
            event_count: 12,
            archived: false,
            is_subagent: false,
            parent_id: String::new(),
            root_id: String::new(),
            label: String::new(),
            state: String::new(),
            task: String::new(),
        }
    }

    fn hit(session: &str, seq: u64, text: &str) -> transcripts::TranscriptHit {
        transcripts::TranscriptHit {
            session_id: session.to_string(),
            title: "some conversation".into(),
            is_subagent: false,
            label: String::new(),
            seq,
            ts_ms: 1_000,
            kind: "user".into(),
            text: text.to_string(),
        }
    }

    fn report(hits: Vec<transcripts::TranscriptHit>) -> transcripts::SearchReport {
        let n = hits.len() as u64;
        transcripts::SearchReport {
            hits,
            matched_conversations: 1,
            total_matches: n,
            scanned_conversations: 3,
            capped: false,
            incomplete: false,
        }
    }

    #[test]
    fn every_listed_conversation_carries_the_id_the_next_call_needs() {
        // The one thing a catalogue must not omit. A pretty listing with no ids
        // in it leaves the model unable to act on what it just found.
        let out = format_conversations(&[conversation("abc-123", "Fixing the build")], NOW);
        assert!(out.contains("abc-123"), "{out}");
        assert!(out.contains("Fixing the build"), "{out}");
        assert!(out.contains("12 events"), "{out}");
    }

    #[test]
    fn a_sub_agent_is_labelled_as_one_and_shows_its_brief() {
        let mut child = conversation("child-1", "");
        child.is_subagent = true;
        child.label = "scout".into();
        child.task = "go and read the parser".into();
        child.state = "done".into();

        let out = format_conversations(&[child], NOW);
        assert!(out.contains("scout"), "{out}");
        assert!(out.contains("sub-agent"), "the kind must be visible: {out}");
        assert!(out.contains("done"), "{out}");
        // The brief, not the auto-derived title, is what says what a child was
        // for — so it is what the row shows.
        assert!(out.contains("go and read the parser"), "{out}");
    }

    #[test]
    fn an_empty_listing_is_a_sentence_not_an_empty_string() {
        let out = format_conversations(&[], NOW);
        assert!(!out.trim().is_empty());
        assert!(out.contains("no conversations"), "{out}");
    }

    #[test]
    fn a_transcript_read_says_how_to_page_on() {
        let entries = vec![
            transcripts::TranscriptEntry {
                seq: 4,
                ts_ms: 1_000,
                kind: "user".into(),
                text: "do the thing".into(),
                elided: 0,
            },
            transcripts::TranscriptEntry {
                seq: 7,
                ts_ms: 1_001,
                kind: "tool-failed".into(),
                text: "no such target".into(),
                elided: 0,
            },
        ];
        let out = format_transcript_entries("### head", &entries);
        assert!(out.contains("[4]") && out.contains("[7]"), "{out}");
        assert!(out.contains("FAILED"), "a failure must look like one: {out}");
        // Paging is only possible if the reply says where it stopped.
        assert!(out.contains("from_seq=7"), "{out}");
    }

    #[test]
    fn a_clipped_entry_admits_what_was_cut() {
        // Otherwise a half-sentence gets quoted back as though it were whole.
        let entries = vec![transcripts::TranscriptEntry {
            seq: 1,
            ts_ms: 1,
            kind: "assistant".into(),
            text: "the beginning of a long answer".into(),
            elided: 4_000,
        }];
        let out = format_transcript_entries("head", &entries);
        assert!(out.contains("4000 more chars"), "{out}");
    }

    #[test]
    fn an_empty_range_distinguishes_itself_from_an_empty_conversation() {
        let out = format_transcript_entries("head", &[]);
        assert!(out.contains("from_seq") || out.contains("empty"), "{out}");
    }

    #[test]
    fn a_capped_search_says_so_and_says_what_to_do() {
        // The failure this guards against is silent: a capped result reads
        // exactly like a complete one, and gets trusted as exhaustive.
        let mut r = report(vec![hit("s1", 1, "needle")]);
        r.capped = true;
        r.total_matches = 900;
        let out = format_search_report(&r);
        assert!(out.contains("Stopped at the first"), "{out}");
        assert!(out.contains("900"), "{out}");
        assert!(out.contains("Narrow"), "it must say what to do: {out}");
    }

    #[test]
    fn an_incomplete_scan_is_distinguished_from_a_capped_one() {
        // Different causes needing different remedies: too many hits versus too
        // many conversations to have opened them all.
        let mut r = report(vec![hit("s1", 1, "needle")]);
        r.incomplete = true;
        let out = format_search_report(&r);
        assert!(out.contains("oldest are"), "{out}");
        assert!(!out.contains("Stopped at the first"), "{out}");
    }

    #[test]
    fn hits_are_grouped_under_the_conversation_they_came_from() {
        let out = format_search_report(&report(vec![
            hit("session-aaa", 1, "first hit"),
            hit("session-aaa", 9, "second hit"),
            hit("session-bbb", 2, "third hit"),
        ]));
        // Once per conversation, not once per hit: the id is the expensive part
        // of the output and repeating it per line is most of the tokens.
        assert_eq!(out.matches("session-aaa").count(), 1, "{out}");
        assert_eq!(out.matches("session-bbb").count(), 1, "{out}");
        assert!(out.contains("conversation_read"), "say how to open one: {out}");
    }

    #[test]
    fn no_matches_suggests_a_way_to_widen_the_search() {
        // An empty result is where a model most needs a next move, and the two
        // flags that would have found the answer are not obvious from a schema.
        let out = format_search_report(&transcripts::SearchReport {
            hits: vec![],
            matched_conversations: 0,
            total_matches: 0,
            scanned_conversations: 5,
            capped: false,
            incomplete: false,
        });
        assert!(out.contains("no matches"), "{out}");
        assert!(out.contains("include_tool_output"), "{out}");
    }

    #[test]
    fn a_multiline_hit_is_flattened_so_one_hit_looks_like_one_hit() {
        let out = one_line("first line\n\nsecond line", 100);
        assert_eq!(out, "first line second line");
    }

    #[test]
    fn an_age_is_rendered_in_the_coarsest_useful_unit() {
        assert_eq!(ago(NOW - 30_000, NOW), "just now");
        assert_eq!(ago(NOW - 600_000, NOW), "10m ago");
        assert_eq!(ago(NOW - 7_200_000, NOW), "2h ago");
        assert_eq!(ago(NOW - 172_800_000, NOW), "2d ago");
        // A zero timestamp and a clock that disagrees must not render as
        // "1971220d ago" or underflow.
        assert_eq!(ago(0, NOW), "just now");
        assert_eq!(ago(NOW + 5_000, NOW), "just now");
    }

    #[test]
    fn an_absent_flag_reads_as_the_conservative_default() {
        let args = json!({ "include_subagents": true });
        assert!(flag(&args, "include_subagents"));
        assert!(!flag(&args, "include_tool_output"));
        assert!(!flag(&json!({}), "include_archived"));
    }
}

/// Tests for the delegation tools' own logic: the brief-length gate, the wait
/// plan, and the formatters a parent reads its children through.
#[cfg(test)]
mod delegation_tests {
    use super::{
        check_brief, format_child, format_children, format_profiles, wait_plan, MIN_BRIEF,
        STALE_CHILD_MS,
    };
    use crate::thetis::grip::delegation;
    use serde_json::json;

    /// A finished child, for the formatters to render.
    fn child(id: &str, state: &str, answer: &str) -> delegation::SubagentInfo {
        delegation::SubagentInfo {
            id: id.to_string(),
            label: format!("{id}-label"),
            task: "a brief long enough to be accepted by the length check".to_string(),
            profile: String::new(),
            model: String::new(),
            mode: String::new(),
            state: state.to_string(),
            answer: answer.to_string(),
            detail: String::new(),
            cost_usd: 0.0,
            created_ms: 1_000,
            finished_ms: 0,
            events: 0,
            activity_ms: 0,
        }
    }

    /// The clock every formatting test reads against. Far enough past
    /// `created_ms` that elapsed time is a real number.
    const NOW: u64 = 1_000 + 60_000;

    // --- the brief ---------------------------------------------------------

    #[test]
    fn a_one_line_brief_is_refused() {
        let err = check_brief("look into the auth bug").expect_err("a one-liner must be refused");
        assert!(
            err.contains("cannot see this conversation"),
            "the refusal has to explain *why* more detail is needed, or the model \
             will simply pad the string to length: {err}"
        );
    }

    #[test]
    fn whitespace_does_not_pad_a_brief_to_length() {
        assert!(
            check_brief(&format!("{}{}", " ".repeat(200), "too short")).is_err(),
            "the check must trim first, or indentation alone satisfies it"
        );
    }

    #[test]
    fn a_real_brief_is_accepted() {
        assert!(check_brief(
            "Find every call site of `commit_worktree` under crates/ and report each as \
             path:line with one line of why it is there. Do not change any file."
        )
        .is_ok());
    }

    // A short brief in a multi-byte script must be measured the same way as an
    // English one: counting bytes would let ASCII through at 40 characters
    // while demanding only 13 of Japanese.
    #[test]
    fn the_floor_counts_characters_not_bytes() {
        let short = "調査".repeat(MIN_BRIEF / 2 - 1);
        assert!(short.len() > MIN_BRIEF, "the test string must be byte-long");
        assert!(
            check_brief(&short).is_err(),
            "{} chars ({} bytes) is under the floor and must be refused",
            short.chars().count(),
            short.len()
        );
    }

    // --- the wait plan -----------------------------------------------------

    #[test]
    fn with_children_running_a_bare_wait_means_all_of_them() {
        let (until, _) = wait_plan(&json!({}), true, 1800).expect("a bare wait must be allowed");
        assert_eq!(
            until, "all",
            "`wait` with no arguments is what a parent writes after fanning out, and \
             refusing it costs a round trip"
        );
    }

    #[test]
    fn with_nothing_running_a_bare_wait_is_a_sleep_and_needs_a_duration() {
        let err = wait_plan(&json!({}), false, 1800)
            .expect_err("a sleep with no duration has nothing to end it");
        assert!(err.contains("timeout_secs"), "{err}");
    }

    #[test]
    fn a_sleep_with_a_duration_is_fine() {
        let (until, secs) = wait_plan(&json!({"timeout_secs": 30}), false, 1800).unwrap();
        assert_eq!((until.as_str(), secs), ("time", 30));
    }

    #[test]
    fn an_explicit_predicate_beats_the_default() {
        let (until, _) = wait_plan(&json!({"until": "first_failure"}), true, 1800).unwrap();
        assert_eq!(until, "first_failure");
    }

    #[test]
    fn a_predicate_wait_gets_the_cap_as_its_deadline_when_none_is_asked() {
        let (_, secs) = wait_plan(&json!({"until": "all"}), true, 900).unwrap();
        assert_eq!(
            secs, 900,
            "a wait with no deadline cannot be distinguished from a hung turn"
        );
    }

    #[test]
    fn an_over_long_request_is_clamped_rather_than_refused() {
        let (_, secs) = wait_plan(&json!({"until": "all", "timeout_secs": 99_999}), true, 900)
            .expect("asking for too long is a mistake to correct, not to fail on");
        assert_eq!(secs, 900);
    }

    // --- what the parent reads --------------------------------------------

    // The whole point of delegating is that the answer arrives without the work
    // behind it. If the answer needed a second call to read, the parent would
    // pay a round trip for every child.
    #[test]
    fn a_finished_child_shows_its_answer_inline() {
        let out = format_child(&child("c1", "done", "the answer is 12"), NOW);
        assert!(out.contains("the answer is 12"), "{out}");
    }

    #[test]
    fn a_running_child_says_so_instead_of_showing_nothing() {
        let out = format_child(&child("c1", "running", ""), NOW);
        assert!(out.contains("Still working"), "{out}");
    }

    // The failure this exists to prevent: seven children working hard rendered
    // as seven identical lines with no clock and no cost, so the parent read
    // them as hung and cancelled every one.
    #[test]
    fn a_running_child_shows_progress_not_just_that_it_is_running() {
        let mut row = child("c1", "running", "");
        row.events = 143;
        row.activity_ms = NOW - 5_000;
        row.cost_usd = 4.25;
        let out = format_child(&row, NOW);
        assert!(out.contains("143"), "the log has to be countable: {out}");
        assert!(out.contains("just now"), "last activity has to show: {out}");
        assert!(out.contains("$4.25"), "live spend has to show: {out}");
        assert!(out.contains("60s"), "a running child needs a clock: {out}");
    }

    #[test]
    fn a_child_that_has_gone_quiet_is_called_out() {
        let now = 1_000 + STALE_CHILD_MS * 2;
        let mut row = child("c1", "running", "");
        row.events = 12;
        row.activity_ms = 1_000;
        let out = format_child(&row, now);
        assert!(
            out.contains("Nothing for a while"),
            "a long silence is the one thing worth flagging: {out}"
        );
    }

    // A silence that is merely long is not evidence of death, and saying so
    // keeps the warning from being the reason a parent cancels healthy work.
    #[test]
    fn going_quiet_suggests_looking_rather_than_cancelling() {
        let now = 1_000 + STALE_CHILD_MS * 2;
        let mut row = child("c1", "running", "");
        row.events = 12;
        row.activity_ms = 1_000;
        let out = format_child(&row, now);
        assert!(out.contains("agent_transcript"), "{out}");
    }

    // A failed progress read is not the same fact as a child that has done
    // nothing, and conflating them would recreate the original bug.
    #[test]
    fn an_unavailable_progress_reading_says_so() {
        let out = format_child(&child("c1", "running", ""), NOW);
        assert!(out.contains("no progress reading available"), "{out}");
    }

    #[test]
    fn a_failure_reason_is_not_swallowed() {
        let mut row = child("c1", "failed", "");
        row.detail = "ran out of iterations".to_string();
        let out = format_child(&row, NOW);
        assert!(out.contains("ran out of iterations"), "{out}");
        assert!(out.contains("failed"), "the state must be visible too: {out}");
    }

    // The header is what the model reads first and often all it reads, so the
    // counts that change a decision — is anything still running, did anything
    // fail — have to be in it.
    #[test]
    fn the_summary_line_counts_running_and_failed() {
        let rows = vec![
            child("a", "done", "A"),
            child("b", "running", ""),
            child("c", "failed", ""),
            child("d", "cancelled", ""),
        ];
        let head = format_children(&rows, NOW)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(head.contains("4 sub-agent"), "{head}");
        assert!(head.contains("1 running"), "{head}");
        assert!(
            head.contains("2 not completed"),
            "cancelled counts with failed — neither produced an answer: {head}"
        );
    }

    #[test]
    fn no_children_reads_as_a_sentence_not_an_empty_string() {
        assert!(format_children(&[], NOW).contains("not spawned"));
    }

    #[test]
    fn every_child_appears_in_the_listing() {
        let rows = vec![child("a", "done", "A answer"), child("b", "done", "B answer")];
        let out = format_children(&rows, NOW);
        assert!(out.contains("A answer") && out.contains("B answer"), "{out}");
    }

    // A deployment with no profiles still delegates, so the empty case must
    // read as an explanation rather than a blank.
    #[test]
    fn no_profiles_explains_what_happens_anyway() {
        let limits = delegation::DelegationLimits {
            max_children: 8,
            max_wait_secs: 1800,
            max_result_bytes: 24_576,
        };
        let out = format_profiles(&[], &limits);
        assert!(out.contains("No profiles"), "{out}");
        assert!(
            out.contains("8 running at once"),
            "the limits are what stop a model fanning out past the cap: {out}"
        );
    }

    #[test]
    fn a_profile_shows_how_it_differs() {
        let limits = delegation::DelegationLimits {
            max_children: 4,
            max_wait_secs: 60,
            max_result_bytes: 1_024,
        };
        let out = format_profiles(
            &[delegation::AgentProfileInfo {
                id: "scout".to_string(),
                label: "Scout".to_string(),
                description: "Reads and reports.".to_string(),
                model: "anthropic/claude-sonnet-5".to_string(),
                mode: "plan".to_string(),
            }],
            &limits,
        );
        assert!(out.contains("scout"), "{out}");
        assert!(out.contains("claude-sonnet-5"), "{out}");
        assert!(
            out.contains("plan mode"),
            "a read-only profile must be recognisable as one: {out}"
        );
    }
}

/// Tests for the classification invariant that `all_builtins` exists to keep.
///
/// Neither `available` nor `all_builtins` can be called here: both probe host
/// capabilities, and a host import traps outside wasm. But the group functions
/// they are assembled from are pure, so the invariant can be checked against
/// those — which is also where it breaks, since the failure mode is a tool
/// defined inline in `available` rather than in a named function.
#[cfg(test)]
mod coverage_tests {
    use super::*;

    /// Every capability-gated tool set, as `all_builtins` sees them. A new
    /// group must be added here and to `all_builtins` together.
    fn gated() -> Vec<ToolDef> {
        let mut all = Vec::new();
        all.extend(sandbox_tools());
        all.extend(devkit_tools());
        all.extend(filesystem_tools());
        all.extend(terminal_tools());
        all.extend(git_tools());
        all.extend(ssh_host_tools());
        all.extend(subagent_tools());
        all.extend(restart_tools());
        all
    }

    /// A capability-gated tool must live in a named function, because that is
    /// the only kind `all_builtins` can add back when the capability is off.
    ///
    /// This is the bug that hid in `restart_orchestrator`: defined inline under
    /// `if restart_available()`, it silently left the builtin list in any
    /// deployment without the control capability. Nothing failed loudly — the
    /// name merely became unclassifiable, so `is_mutating` fell through to the
    /// component path and guessed, and the group table reported the `selfmod`
    /// entry for it as naming a tool that does not exist.
    ///
    /// Asserted from the group table's side, so it holds for tools not yet
    /// written: every member of a group whose tools are capability-gated has to
    /// be reachable without any capability being live.
    #[test]
    fn every_gated_group_member_is_reachable_without_its_capability() {
        let reachable: Vec<&str> = gated().iter().map(|t| t.name).collect();
        // The groups whose members are all capability-gated. `core`, `skills`,
        // `files` and `config` are unconditional, and the rest are hot-loaded
        // components with no members in the table.
        for id in ["sandbox", "shell", "ssh", "selfmod", "branch", "subagents"] {
            let group = crate::groups::all()
                .iter()
                .find(|g| g.id == id)
                .unwrap_or_else(|| panic!("no `{id}` group"));
            for member in group.members {
                assert!(
                    reachable.contains(member),
                    "`{member}` is in the `{id}` group but no named group function \
                     yields it, so `all_builtins` cannot classify it when the \
                     capability is off. Move it out of the inline block in \
                     `available` into a named function, and add that function to \
                     both `available` and `all_builtins`."
                );
            }
        }
    }

    /// `restart_orchestrator` mutates, and a read-only mode must withhold it.
    /// It is the tool whose flag matters most: it is how a kernel change goes
    /// live.
    #[test]
    fn restarting_is_classified_as_mutating() {
        let restart = restart_tools();
        assert_eq!(restart.len(), 1);
        assert_eq!(restart[0].name, "restart_orchestrator");
        assert!(restart[0].mutating);
    }

    /// No name may be yielded by two group functions, or `group_of` and the
    /// mutating flag both become order-dependent.
    #[test]
    fn no_gated_tool_is_defined_twice() {
        let all = gated();
        for (i, tool) in all.iter().enumerate() {
            if let Some(dup) = all[..i].iter().find(|t| t.name == tool.name) {
                panic!("`{}` is defined twice", dup.name);
            }
        }
    }
}

/// Tests for the parts of `ask_user` the turn loop depends on.
///
/// The loop ends the turn when a call named [`ASK_USER`] *succeeds*, so two
/// things have to hold: the advertised name must equal the constant the loop
/// matches on, and validation must reject a malformed question rather than
/// returning `Ok`. Both are checked here because both fail silently — a rename
/// would simply stop the pause happening, and an `Ok` on a question that was
/// never shown would hang the conversation waiting for an answer.
#[cfg(test)]
mod ask_user_tests {
    use super::{ask_user, ASK_USER, MAX_OPTIONS, MAX_QUESTIONS};
    use serde_json::json;

    // `available()` is deliberately not called here: building the tool list
    // reads capability flags through host imports, which do not exist outside
    // wasm, and calling it from a host test aborts the process rather than
    // failing. The constant is what both the tool definition and the turn loop
    // refer to, so pinning its value is what actually guards against drift.
    #[test]
    fn the_wire_name_is_pinned() {
        assert_eq!(
            ASK_USER, "ask_user",
            "the web form, the Discord flow and the turn loop all key off this exact string"
        );
    }

    #[test]
    fn a_well_formed_question_succeeds_so_the_turn_pauses() {
        let result = ask_user(&json!({
            "questions": [{ "question": "Ship it?", "options": ["yes", "no"] }]
        }));
        assert!(result.is_ok(), "a valid question must report success");
    }

    #[test]
    fn an_open_question_needs_no_options() {
        assert!(ask_user(&json!({
            "questions": [{ "question": "What broke?", "type": "open" }]
        }))
        .is_ok());
    }

    // Each of these must be an `Err`. The loop only pauses on success, so a
    // rejection here is what keeps a malformed call from stalling the
    // conversation on a form the user never saw.
    #[test]
    fn a_choice_question_with_no_options_is_rejected() {
        assert!(ask_user(&json!({
            "questions": [{ "question": "Pick one", "type": "choice" }]
        }))
        .is_err());
    }

    #[test]
    fn a_question_with_no_text_is_rejected() {
        assert!(ask_user(&json!({ "questions": [{ "question": "   " }] })).is_err());
    }

    #[test]
    fn an_empty_or_missing_question_list_is_rejected() {
        assert!(ask_user(&json!({ "questions": [] })).is_err());
        assert!(ask_user(&json!({})).is_err());
    }

    #[test]
    fn too_many_questions_is_rejected() {
        let questions: Vec<_> = (0..MAX_QUESTIONS + 1)
            .map(|i| json!({ "question": format!("q{i}"), "type": "open" }))
            .collect();
        assert!(ask_user(&json!({ "questions": questions })).is_err());
    }

    #[test]
    fn too_many_options_is_rejected() {
        let options: Vec<_> = (0..MAX_OPTIONS + 1).map(|i| format!("o{i}")).collect();
        assert!(ask_user(&json!({
            "questions": [{ "question": "Pick", "options": options }]
        }))
        .is_err());
    }

    #[test]
    fn an_unknown_type_is_rejected_rather_than_guessed() {
        assert!(ask_user(&json!({
            "questions": [{ "question": "Eh?", "type": "dropdown" }]
        }))
        .is_err());
    }
}
