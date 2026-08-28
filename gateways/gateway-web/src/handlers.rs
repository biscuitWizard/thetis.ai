//! One handler per client action.
//!
//! Adding a capability to the chat surface means adding a function here and a
//! line to the table in `dispatch` — nothing else in the gateway changes.

use crate::thetis::grip::session as host;
use crate::thetis::grip::skills_view as view;
use crate::thetis::grip::sys;
use crate::thetis::grip::types::Attachment;
use crate::render;
use crate::{GatewayAction, OutboundEvent};
use serde_json::{json, Value};

pub fn reply(value: Value) -> GatewayAction {
    GatewayAction::Reply(value.to_string())
}

pub fn error(message: impl AsRef<str>) -> GatewayAction {
    reply(json!({ "type": "error", "message": message.as_ref() }))
}

/// Routes an inbound frame to its handler.
pub fn dispatch(frame: &Value) -> Vec<GatewayAction> {
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").and_then(Value::as_str);
    let previous = frame.get("previous").and_then(Value::as_str);

    match kind {
        "hello" => vec![catalog(), sessions()],
        "list" => vec![sessions()],
        "catalog" => vec![catalog()],

        "open" => match id {
            Some(session) => open(session, previous),
            None => vec![error("open requires an id")],
        },

        "new" => new_session(frame, previous),
        "send" => match id {
            Some(session) => send(session, frame),
            None => vec![error("send requires an id")],
        },

        "rename" => match (id, frame.get("title").and_then(Value::as_str)) {
            (Some(session), Some(title)) if !title.trim().is_empty() => {
                host::rename_session(session, title.trim());
                vec![sessions()]
            }
            _ => vec![error("rename requires an id and a title")],
        },

        "archive" => match id {
            Some(session) => vec![
                {
                    host::archive_session(session, true);
                    GatewayAction::Unsubscribe(session.to_string())
                },
                sessions(),
            ],
            None => vec![error("archive requires an id")],
        },

        "unarchive" => match id {
            Some(session) => {
                host::archive_session(session, false);
                vec![sessions()]
            }
            None => vec![error("unarchive requires an id")],
        },

        "set-mode" => match (id, frame.get("mode").and_then(Value::as_str)) {
            (Some(session), Some(mode)) => {
                host::set_session_mode(session, mode);
                vec![session_settings(session), sessions()]
            }
            _ => vec![error("set-mode requires an id and a mode")],
        },

        "skills" => match id {
            Some(session) => vec![skills(session)],
            None => vec![error("skills requires an id")],
        },

        "tools" => match id {
            Some(session) => vec![tools(session)],
            None => vec![error("tools requires an id")],
        },

        "set-model" => match (id, frame.get("model").and_then(Value::as_str)) {
            (Some(session), Some(model)) => {
                host::set_session_model(session, model);
                vec![session_settings(session), sessions()]
            }
            _ => vec![error("set-model requires an id and a model")],
        },

        // The model catalogue. Editing it is deliberately not a config write:
        // `thetis.toml` is only read at startup, so a model added there could
        // not be picked until a restart. The overlay lives in the host KV store
        // instead, which means a slug typed here is selectable in the same
        // breath - and `set-session-model` accepts any slug, so it works.
        "models" => vec![catalog()],
        "model-save" => save_model(frame, id),
        "model-remove" => remove_model(frame, id),
        "model-restore" => restore_model(frame, id),

        other => vec![error(format!("unknown frame type: {other}"))],
    }
}

// --- the model catalogue ----------------------------------------------------

/// Where the overlay is kept. Global scope: the catalogue is a property of the
/// installation, not of one conversation.
const MODEL_KEY: &str = "gateway.web.models";
/// Ceiling on stored entries, so a runaway client cannot grow the record without
/// bound. Well past any plausible hand-curated list.
const MAX_MODELS: usize = 64;
const MAX_SLUG: usize = 200;
const MAX_LABEL: usize = 80;

/// One user-authored change to the catalogue: a new model, a relabelled one, or
/// a configured one pushed out of the picker.
#[derive(Clone)]
struct Entry {
    id: String,
    label: String,
    hidden: bool,
}

fn load_overlay() -> Vec<Entry> {
    let raw = sys::kv_get("global", MODEL_KEY).unwrap_or_default();
    serde_json::from_str::<Value>(&raw)
        .ok()
        .as_ref()
        .and_then(|v| v.get("entries"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.trim().to_string();
                    if id.is_empty() {
                        return None;
                    }
                    Some(Entry {
                        id,
                        label: item
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        hidden: item
                            .get("hidden")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_overlay(entries: &[Entry]) {
    let payload = json!({
        "entries": entries.iter().map(|e| json!({
            "id": e.id, "label": e.label, "hidden": e.hidden,
        })).collect::<Vec<_>>(),
    });
    sys::kv_put("global", MODEL_KEY, &payload.to_string());
}

/// A model as the picker and the inspector see it.
struct Merged {
    id: String,
    label: String,
    /// "config" straight from thetis.toml, "override" relabelled here,
    /// "custom" added here. The panel needs this to know what restoring means.
    source: &'static str,
    hidden: bool,
}

/// A readable name for a slug that was given none: "anthropic/claude-opus-5"
/// becomes "Claude Opus 5". Better than an empty pill, and the slug is always
/// shown underneath anyway.
///
/// A short vowel-less word is treated as an acronym, so "gpt" comes out "GPT"
/// rather than "Gpt". That is a guess, but it is wrong in the harmless direction
/// on the names providers actually use, and anyone who dislikes the result can
/// type a label.
fn pretty(slug: &str) -> String {
    fn acronym(word: &str) -> bool {
        word.chars().count() <= 4
            && word.chars().any(|c| c.is_ascii_alphabetic())
            && !word.chars().any(|c| "aeiouAEIOU".contains(c))
    }

    let tail = slug.rsplit('/').next().unwrap_or(slug);
    let mut out = String::new();
    // Not on '.': a dot in a model name is nearly always a version, and
    // splitting there turned "llama-3.3-70b" into "Llama 3 3 70B".
    for word in tail.split(['-', '_']).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        if acronym(word) {
            out.extend(word.chars().flat_map(char::to_uppercase));
            continue;
        }
        let mut chars = word.chars();
        match chars.next() {
            Some(first) if word.chars().any(|c| c.is_ascii_alphabetic()) => {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
            _ => out.push_str(word),
        }
    }
    if out.is_empty() {
        slug.to_string()
    } else {
        out
    }
}

/// The configured list with the overlay applied.
///
/// Configured models keep their order and come first, so editing the catalogue
/// never reshuffles what someone is used to; additions land underneath in the
/// order they were made.
fn merged_models() -> Vec<Merged> {
    let overlay = load_overlay();
    let configured = sys::list_models();
    let mut out: Vec<Merged> = Vec::new();

    for m in configured.iter() {
        let edit = overlay.iter().find(|e| e.id == m.id);
        let label = edit
            .map(|e| e.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| {
                if m.label.trim().is_empty() {
                    pretty(&m.id)
                } else {
                    m.label.clone()
                }
            });
        out.push(Merged {
            id: m.id.clone(),
            label,
            source: match edit {
                Some(e) if !e.label.is_empty() => "override",
                _ => "config",
            },
            hidden: edit.map(|e| e.hidden).unwrap_or(false),
        });
    }

    for e in overlay.iter() {
        if configured.iter().any(|m| m.id == e.id) {
            continue;
        }
        out.push(Merged {
            id: e.id.clone(),
            label: if e.label.is_empty() {
                pretty(&e.id)
            } else {
                e.label.clone()
            },
            source: "custom",
            hidden: e.hidden,
        });
    }

    out
}

fn slug_of(frame: &Value, key: &str) -> Option<String> {
    frame
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Accepts anything a provider might plausibly name, and rejects what cannot be
/// a slug at all. Deliberately loose: an id this gateway has never heard of is
/// the normal case, and a wrong one comes back from the provider as a clear
/// error on the next turn - a better place to find out than a refused click.
fn check_slug(raw: &str) -> Result<String, String> {
    let slug = raw.trim();
    if slug.is_empty() {
        return Err("a model needs a slug, e.g. anthropic/claude-sonnet-4.5".into());
    }
    if slug.chars().count() > MAX_SLUG {
        return Err(format!("that slug is longer than {MAX_SLUG} characters"));
    }
    if slug.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a model slug has no spaces in it".into());
    }
    Ok(slug.to_string())
}

/// Every reply that changes the catalogue sends the whole thing back, and to
/// every tab on this conversation rather than only the one that asked - so a
/// second window never keeps offering a model that has just gone.
fn catalog_replies(session: Option<&str>) -> Vec<GatewayAction> {
    let mut actions = vec![catalog()];
    if let Some(session) = session {
        if let GatewayAction::Reply(frame) = catalog() {
            actions.push(GatewayAction::Broadcast(crate::BroadcastFrame {
                session_id: session.to_string(),
                frame,
            }));
        }
    }
    actions
}

/// Adds a model, or edits one. `previous` set to a different slug is a rename:
/// the old entry is retired in the same call, so the picker never shows both.
fn save_model(frame: &Value, session: Option<&str>) -> Vec<GatewayAction> {
    let slug = match check_slug(frame.get("slug").and_then(Value::as_str).unwrap_or("")) {
        Ok(slug) => slug,
        Err(why) => return vec![error(why)],
    };
    let label: String = frame
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_LABEL)
        .collect();

    let configured = sys::list_models();
    let mut overlay = load_overlay();

    if let Some(previous) = slug_of(frame, "previous").filter(|p| *p != slug) {
        retire(&mut overlay, &configured, &previous);
    }

    // A configured model given back its own label needs no overlay entry at
    // all; dropping it keeps the record to genuine differences.
    let configured_label = configured
        .iter()
        .find(|m| m.id == slug)
        .map(|m| m.label.trim().to_string());
    let redundant = matches!(&configured_label, Some(l) if label.is_empty() || *l == label);

    match overlay.iter_mut().find(|e| e.id == slug) {
        Some(entry) if redundant => {
            entry.label = String::new();
            entry.hidden = false;
        }
        Some(entry) => {
            entry.label = label;
            entry.hidden = false;
        }
        None if redundant => {}
        None => {
            if overlay.len() >= MAX_MODELS {
                return vec![error(format!(
                    "the catalogue already holds {MAX_MODELS} edits; remove one first"
                ))];
            }
            overlay.push(Entry {
                id: slug,
                label,
                hidden: false,
            });
        }
    }

    overlay.retain(|e| !(e.label.is_empty() && !e.hidden && configured.iter().any(|m| m.id == e.id)));
    save_overlay(&overlay);
    catalog_replies(session)
}

/// Takes a model out of the picker. A configured one cannot be deleted - it is
/// in the file - so it is marked hidden and can be restored.
fn retire(overlay: &mut Vec<Entry>, configured: &[sys::ModelInfo], slug: &str) {
    if configured.iter().any(|m| m.id == slug) {
        match overlay.iter_mut().find(|e| e.id == slug) {
            Some(entry) => entry.hidden = true,
            None => overlay.push(Entry {
                id: slug.to_string(),
                label: String::new(),
                hidden: true,
            }),
        }
    } else {
        overlay.retain(|e| e.id != slug);
    }
}

fn remove_model(frame: &Value, session: Option<&str>) -> Vec<GatewayAction> {
    let Some(slug) = slug_of(frame, "slug") else {
        return vec![error("model-remove requires a slug")];
    };
    let configured = sys::list_models();
    let mut overlay = load_overlay();
    retire(&mut overlay, &configured, &slug);
    save_overlay(&overlay);
    catalog_replies(session)
}

/// Forgets every edit to one slug, putting a configured model back as the file
/// has it. On a custom model this is the same as removing it.
fn restore_model(frame: &Value, session: Option<&str>) -> Vec<GatewayAction> {
    let Some(slug) = slug_of(frame, "slug") else {
        return vec![error("model-restore requires a slug")];
    };
    let mut overlay = load_overlay();
    overlay.retain(|e| e.id != slug);
    save_overlay(&overlay);
    catalog_replies(session)
}

// --- frames -----------------------------------------------------------------

/// What the pickers offer, and what the models inspector shows.
///
/// `models` is what the picker lists; `models_hidden` is what has been pushed
/// out of it, which the inspector needs so a hidden model can be brought back.
pub fn catalog() -> GatewayAction {
    let merged = merged_models();
    let entry = |m: &Merged| {
        json!({
            "id": m.id,
            "label": m.label,
            "source": m.source,
            "hidden": m.hidden,
        })
    };

    reply(json!({
        "type": "catalog",
        "models": merged.iter().filter(|m| !m.hidden).map(entry).collect::<Vec<_>>(),
        "models_hidden": merged.iter().filter(|m| m.hidden).map(entry).collect::<Vec<_>>(),
        "modes": sys::list_modes().iter().map(|m| json!({
            "id": m.id, "label": m.label, "description": m.description,
        })).collect::<Vec<_>>(),
    }))
}

pub fn sessions() -> GatewayAction {
    // Archived ones included: the sidebar shows them in their own collapsed
    // section, which is what makes them reachable without /admin.
    reply(json!({
        "type": "sessions",
        "sessions": host::list_sessions(true),
    }))
}

/// The skill corpus as an inspector: the whole tree, which briefs are always
/// present, and what retrieval chose for this conversation.
///
/// Read-only by construction. This comes from `skills-view`, which has no way
/// to write, because a chat surface renders skills and the agent authors them.
/// There is nothing to toggle any more: what a conversation can see is decided
/// by ranking its opening message, not by the user ticking boxes.
fn skills(session_id: &str) -> GatewayAction {
    let pinned = view::pinned(session_id);
    let universal = view::universal();

    let card = |c: &view::SkillCard| {
        // Depth is not on the card, but the id encodes it, so the tree view can
        // indent without the host having to say so.
        let depth = c.id.matches('/').count();
        json!({
            "id": c.id,
            "parent": c.parent,
            "depth": depth,
            "name": c.name,
            "brief": c.brief,
            "when_to_use": c.when_to_use,
            "tags": c.tags,
            "children": c.children,
            "resources": c.resources,
            "universal": c.universal,
            "score": c.score,
            "how": c.how,
        })
    };

    reply(json!({
        "type": "skills",
        "session": session_id,
        "all": view::all().iter().map(card).collect::<Vec<_>>(),
        "universal": universal.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        "retrieved": pinned.iter().map(card).collect::<Vec<_>>(),
        "diagnostics": view::lint().iter().map(|d| json!({
            "id": d.id,
            "severity": d.severity,
            "message": d.message,
        })).collect::<Vec<_>>(),
    }))
}

/// The domain a tool belongs to, so the surface can be shown grouped by what a
/// tool touches rather than as one long flat list. Derived from the name —
/// names are stable and already domain-scoped — with a plain "Other" fallback
/// for anything unrecognised, such as a new component or a connected MCP tool.
fn tool_group(name: &str) -> &'static str {
    match name {
        "read_file" | "write_file" | "read_path" | "write_path" | "list_path"
        | "delete_path" | "edit_path" | "search_files" | "find_files" => "Files",
        "exec" => "Shell",
        "remember" | "recall" => "Memory",
        "list_config" | "read_config" | "set_config" | "config-probe" => "Configuration",
        "new_tool" | "read_code" | "write_code" | "patch_code" | "list_code"
        | "add_dependency" | "remove_dependency" | "list_dependencies"
        | "restart_orchestrator" => "Code & tools",
        "update_from_trunk" | "reset_branch" | "complete_merge" | "abort_merge" => {
            "Version control"
        }
        // The ssh host registry belongs with the terminal: its whole purpose is
        // naming a machine `terminal_open` can open a session on.
        _ if name.starts_with("terminal_") || name.starts_with("ssh_host") => "Shell",
        _ if name.starts_with("skill_") => "Skills",
        _ if name.starts_with("branch_") => "Version control",
        _ if name.starts_with("web-") || name.starts_with("web_") => "Web",
        _ if name.starts_with("notion-") => "Notion",
        // The git-* components and the git_clone built-in. Kept apart from
        // "Version control", which is this conversation's own sandbox branch —
        // a different thing from a remote repository on GitHub.
        _ if name.starts_with("git-") || name.starts_with("git_") => "Git",
        _ => "Other",
    }
}

/// Exactly the tools the agent would offer for this conversation's mode.
fn tools(session_id: &str) -> GatewayAction {
    reply(json!({
        "type": "tools",
        "session": session_id,
        "tools": host::available_tools(session_id).iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "schema": t.args_schema_json,
            "capabilities": t.capabilities,
            "group": tool_group(&t.name),
        })).collect::<Vec<_>>(),
    }))
}

fn session_settings(session_id: &str) -> GatewayAction {
    let meta = host::get_session(session_id);
    reply(json!({
        "type": "settings",
        "session": session_id,
        "mode": meta.as_ref().map(|m| m.mode.clone()).unwrap_or_default(),
        "model": meta.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
    }))
}

fn history(session_id: &str) -> GatewayAction {
    let meta = host::get_session(session_id);
    let events: Vec<Value> = host::events(session_id, 0)
        .iter()
        .filter_map(|record| {
            render::event(&OutboundEvent {
                session_id: session_id.to_string(),
                seq: Some(record.seq),
                ts_ms: record.ts_ms,
                event: record.event.clone(),
            })
        })
        .collect();

    reply(json!({
        "type": "history",
        "session": session_id,
        "title": meta.as_ref().map(|m| m.title.clone()).unwrap_or_default(),
        "mode": meta.as_ref().map(|m| m.mode.clone()).unwrap_or_default(),
        "model": meta.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
        "events": events,
    }))
}

// --- actions ----------------------------------------------------------------

fn open(session_id: &str, previous: Option<&str>) -> Vec<GatewayAction> {
    let mut actions = Vec::new();
    if let Some(prev) = previous {
        if prev != session_id {
            actions.push(GatewayAction::Unsubscribe(prev.to_string()));
        }
    }
    actions.push(GatewayAction::Subscribe(session_id.to_string()));
    actions.push(history(session_id));
    actions
}

fn new_session(frame: &Value, previous: Option<&str>) -> Vec<GatewayAction> {
    let title = frame
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let id = host::create_session(title.as_deref());

    let mut actions = vec![sessions()];
    actions.extend(open(&id, previous));
    actions.push(reply(json!({ "type": "opened", "session": id })));
    actions
}

fn send(session_id: &str, frame: &Value) -> Vec<GatewayAction> {
    let text = frame
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let attachments = parse_attachments(frame);

    // An empty message with no files is a stray keypress, not a turn.
    if text.is_empty() && attachments.is_empty() {
        return Vec::new();
    }

    host::submit(session_id, &text, &attachments);
    // The message itself is not echoed: it returns through the event stream, so
    // the transcript stays a pure function of the log. What is echoed is a bare
    // acknowledgement, because `submit` only returns once the host has the
    // message — for a conversation's first one, after its branch, worktree and
    // worker have been created. The client holds an optimistic row and a locked
    // composer until this arrives, and needs a positive signal to release them
    // that does not depend on the event broadcast reaching this tab.
    vec![reply(json!({ "type": "accepted", "session": session_id }))]
}

fn parse_attachments(frame: &Value) -> Vec<Attachment> {
    frame
        .get("attachments")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(Attachment {
                        name: item.get("name").and_then(Value::as_str)?.to_string(),
                        mime: item.get("mime").and_then(Value::as_str)?.to_string(),
                        data_base64: item.get("data").and_then(Value::as_str)?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
