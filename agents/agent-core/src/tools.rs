//! The agent's tool surface.
//!
//! Tools are advertised only when the capability behind them is actually
//! available, so the model is never offered something that will just fail. As
//! the orchestrator gains capabilities (the sandbox, the dev kit), the flags
//! flip and the tools appear without the agent needing to change.

use crate::thetis::grip::types::{
    CompileReport, ConfigEntry, Dependency, EnvVar, ExecResult, FsEntry, LogLevel, ModTarget,
    SshHostInfo, TerminalOpen, TerminalOutput, ToolManifest,
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
        tools.extend(git_tools());
    }
    // Classification must cover it either way: whether `ssh_host` is offered
    // depends on config, but it is mutating in every configuration.
    if !(terminal_available() && terminal::ssh_available()) {
        tools.extend(ssh_host_tools());
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
        // git_clone runs the credential script in a shell, so it is offered on
        // the same condition as the terminal itself.
        tools.extend(git_tools());
        if terminal::ssh_available() {
            tools.extend(ssh_host_tools());
        }
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
                 the working directory and environment carry over. Pass `host` to open the \
                 session on a remote machine over ssh, naming a host you registered with \
                 `ssh_host`.\n\nThe terminal is for running programs — builds, tests, git, \
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
                         `ssh_host action=list`.",
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

/// The named-host registry, offered only when remote sessions are actually
/// possible — a tool for managing hosts nothing can connect to is noise.
///
/// Connection details live here once and are referred to by name, so a session
/// is opened with `host: "build-box"` rather than a line of ssh arguments.
fn ssh_host_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "ssh_host",
        description:
            "Manage the named ssh hosts that `terminal_open` can open a session on. Add, edit, \
             rename, remove and list them.\n\nA name here is all `terminal_open` needs, so \
             connection details are stated once. They are kept in a gitignored file that the \
             config loader never reads: it cannot be published by accident, it never appears in \
             `list_config`, and a bad entry cannot stop Thetis from starting. Only the *path* to \
             a private key is stored, never a key or a password — ssh runs with BatchMode on and \
             will not prompt, so a host must authenticate by key.",
        mutating: true,
        parameters: obj(
            json!({
                "action": {
                    "type": "string",
                    "description": "What to do. `set` adds a host or edits one, keeping any \
                                    field you leave out.",
                    "enum": ["list", "get", "set", "remove", "rename"],
                },
                "name": string_prop(
                    "The host's name, for every action except list. Letters, digits, '-', '_' \
                     and '.'.",
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
                    "description": "Allocate a remote terminal. Needed for `terminal_signal` to \
                                    reach that host, and for anything that demands a tty such \
                                    as sudo. Costs echoed commands and prompts mixed into \
                                    output, so leave it off unless you need it.",
                },
                "description": string_prop("A note to yourself about what this host is."),
                "to": string_prop("The new name, for rename."),
            }),
            &["action"],
        ),
    }]
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
        ASK_USER => ask_user(&args),

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

        "ssh_host" => {
            let action = req_str(&args, "action")?;
            match action.as_str() {
                "list" => {
                    let hosts = terminal::ssh_hosts()?;
                    if hosts.is_empty() {
                        return Ok("no ssh hosts defined. Add one with ssh_host action=set, \
                                   giving at least name and host."
                            .to_string());
                    }
                    Ok(hosts
                        .iter()
                        .map(format_ssh_host)
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
                "get" => {
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
                "set" => terminal::ssh_host_set(
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
                    // Always a merge: editing one field of a host should not
                    // silently clear the rest, and there is no plausible reason
                    // to want that.
                    true,
                ),
                "remove" => terminal::ssh_host_remove(&req_str(&args, "name")?),
                "rename" => {
                    terminal::ssh_host_rename(&req_str(&args, "name")?, &req_str(&args, "to")?)
                }
                other => Err(format!(
                    "unknown action {other:?}; use list, get, set, remove or rename"
                )),
            }
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
