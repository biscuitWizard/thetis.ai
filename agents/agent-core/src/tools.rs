//! The agent's tool surface.
//!
//! Tools are advertised only when the capability behind them is actually
//! available, so the model is never offered something that will just fail. As
//! the orchestrator gains capabilities (the sandbox, the dev kit), the flags
//! flip and the tools appear without the agent needing to change.

use crate::thetis::grip::types::{
    CompileReport, ConfigEntry, Dependency, ExecResult, FsEntry, LogLevel, ModTarget,
    TerminalOutput, ToolManifest,
};
use crate::thetis::grip::{
    branch, configuration, control, devkit, hostfs, sandbox, skills, sys, terminal, tooling,
};
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
    if !devkit_available() {
        tools.extend(devkit_tools());
    }
    if !filesystem_available() {
        tools.extend(filesystem_tools());
    }
    if !terminal_available() {
        tools.extend(terminal_tools());
    }
    tools
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

    if sandbox_available() {
        tools.push(ToolDef {
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
        });
        tools.push(ToolDef {
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
        });
        tools.push(ToolDef {
            name: "read_file",
            description: "Read a file from the session's container workspace.",
            mutating: false,
            parameters: obj(json!({ "path": string_prop("Path inside the workspace.") }), &["path"]),
        });
    }

    if devkit_available() {
        tools.extend(devkit_tools());
    }
    if filesystem_available() {
        tools.extend(filesystem_tools());
    }
    if terminal_available() {
        tools.extend(terminal_tools());
    }
    tools.extend(configuration_tools());
    if restart_available() {
        tools.push(ToolDef {
            name: "restart_orchestrator",
            description:
                "Restart this conversation's own runtime — no other conversation notices. \
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
        });
    }

    // In a read-only mode the tools that would change something are simply not
    // offered, rather than offered and then refused.
    if read_only(mode) {
        tools.retain(|t| !t.mutating);
    }

    tools
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
                    "path": string_prop("Directory to search under. Omit for the project root."),
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
                    "path": string_prop("Directory to look under. Omit for the project root."),
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
                 the working directory and environment carry over.\n\nThe terminal is for \
                 running programs — builds, tests, git, package managers, processes, anything \
                 whose output you need. It is not the way to look at or change files: \
                 `search_files`, `find_files`, `read_path` and `edit_path` do that far more \
                 cheaply and reliably than grep, sed and heredocs, and edits made through the \
                 shell are attributed to the user rather than to you.",
            mutating: true,
            parameters: obj(
                json!({ "cwd": string_prop("Directory to start in; defaults to the project root.") }),
                &[],
            ),
        },
        ToolDef {
            name: "terminal_run",
            description:
                "Run a command in an open shell session and wait for it to finish. Returns \
                 whatever it printed. A command that outlives the timeout keeps running; read \
                 the session again later for the rest.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop("Session id from terminal_open."),
                    "command": string_prop("The command line to run."),
                    "timeout_ms": {
                        "type": "integer",
                        "description": "How long to wait. Omit for the configured default.",
                    },
                }),
                &["id", "command"],
            ),
        },
        ToolDef {
            name: "terminal_read",
            description:
                "Read anything a session has printed since the last read, without running \
                 anything. Use it to collect output from a command that timed out.",
            mutating: false,
            parameters: obj(json!({ "id": string_prop("Session id.") }), &["id"]),
        },
        ToolDef {
            name: "terminal_close",
            description: "Close a shell session and stop its process.",
            mutating: true,
            parameters: obj(json!({ "id": string_prop("Session id.") }), &["id"]),
        },
        ToolDef {
            name: "terminal_list",
            description: "List the open shell sessions.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// Tools that edit the running system. Every mutating one rebuilds immediately
/// and returns the compiler's verdict in its result.
fn devkit_tools() -> Vec<ToolDef> {
    let target_prop = json!({
        "type": "string",
        "description": "What to edit: 'self' for your own loop, 'gateway:<name>' for a chat \
                        interface, or 'tool:<name>' for one of your tools.",
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
/// any hot-loaded tool components the orchestrator has registered.
pub fn definitions(mode: &str) -> Vec<Value> {
    let mut defs: Vec<Value> = available(mode)
        .iter()
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

    let args: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("arguments were not valid JSON: {e}"))?;

    match name {
        "remember" => remember(session_id, &args),
        "recall" => recall(session_id, &args),

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

        "terminal_open" => terminal::open(args.get("cwd").and_then(Value::as_str)),
        "terminal_run" => terminal::run(
            &req_str(&args, "id")?,
            &req_str(&args, "command")?,
            args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
        )
        .map(format_terminal),
        "terminal_read" => terminal::read(&req_str(&args, "id")?).map(|text| {
            if text.trim().is_empty() {
                "[nothing new]".to_string()
            } else {
                text
            }
        }),
        "terminal_close" => terminal::close(&req_str(&args, "id")?),
        "terminal_list" => {
            let sessions = terminal::sessions();
            if sessions.is_empty() {
                return Ok("no open sessions".to_string());
            }
            Ok(sessions
                .iter()
                .map(|s| {
                    format!(
                        "{}  {}  {} command(s)  {}",
                        s.id,
                        s.cwd,
                        s.commands,
                        if s.alive { "running" } else { "exited" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
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

// --- formatting -------------------------------------------------------------

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

    if body.file.is_empty() {
        out.push_str(&format!("# {} ({})\n", body.name, body.id));
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
        out.push_str(&format!("\n  {}", card.brief));
        if !card.when_to_use.trim().is_empty() {
            out.push_str(&format!("\n  Use when: {}", card.when_to_use.trim()));
        }
        if !card.children.is_empty() {
            out.push_str(&format!("\n  Nested: {}", card.children.join(", ")));
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
    if result.timed_out {
        out.push_str(
            "\n\n[still running: the timeout elapsed. Use terminal_read to collect the rest.]",
        );
    }
    out
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
