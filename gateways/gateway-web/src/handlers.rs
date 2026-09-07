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
        "hello" => vec![catalog(), sessions(), user_avatar()],
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

        // Overriding which tool groups this conversation offers the model.
        // Replies with the whole `tools` frame rather than an acknowledgement,
        // so the panel redraws from what the store now actually holds instead
        // of from what the client hoped it wrote.
        "tool-groups-set" => match id {
            Some(session) => set_tool_groups(session, frame),
            None => vec![error("tool-groups-set requires an id")],
        },
        "tool-groups-reset" => match id {
            Some(session) => reset_tool_groups(session),
            None => vec![error("tool-groups-reset requires an id")],
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

        // The user's own avatar, shown beside the conversation. Kept in the KV
        // store rather than config for the same reason the model overlay is:
        // `thetis.toml` is read only at startup, so a picture chosen here could
        // not appear until a restart. The agent's avatar is the opposite case —
        // it is identity, set by whoever configures the installation, and is
        // substituted into the markup at serve time.
        "user-avatar" => vec![user_avatar()],
        "user-avatar-set" => set_user_avatar(frame, id),

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

// --- the user's avatar ------------------------------------------------------

/// Where the user's picture is kept. Global scope: it is the person using the
/// installation, not a property of one conversation.
const USER_AVATAR_KEY: &str = "gateway.web.user_avatar";

/// Ceiling on the stored value, in characters. A `data:` URI is base64, so this
/// is roughly a 1.5 MB image — generous for a portrait, and far enough under the
/// 16 MiB websocket cap that the frame carrying it can never be the thing that
/// trips it. The client downscales before uploading; this is the backstop.
const MAX_AVATAR_CHARS: usize = 2_000_000;

/// The stored picture, or an empty string when there is none. Empty is a real
/// answer rather than a missing one: it selects the drawn fallback mark.
pub fn user_avatar() -> GatewayAction {
    reply(json!({
        "type": "user-avatar",
        "avatar": sys::kv_get("global", USER_AVATAR_KEY).unwrap_or_default(),
    }))
}

/// Stores a new picture, or clears it when `avatar` is empty.
///
/// Replies with the whole `user-avatar` frame, and broadcasts it to every tab on
/// the open conversation, so a second window is not left showing the old face.
fn set_user_avatar(frame: &Value, session: Option<&str>) -> Vec<GatewayAction> {
    let raw = frame
        .get("avatar")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();

    if raw.chars().count() > MAX_AVATAR_CHARS {
        return vec![error(
            "that image is too large — pick one under about 1.5 MB",
        )];
    }
    // Only a `data:` image or an http(s) URL. Anything else — `javascript:`
    // above all — would end up in a `src` attribute, so it is refused here
    // rather than trusted to the client's own escaping.
    let allowed = raw.is_empty()
        || raw.starts_with("data:image/")
        || raw.starts_with("https://")
        || raw.starts_with("http://");
    if !allowed {
        return vec![error("an avatar must be an image file or an http(s) URL")];
    }

    sys::kv_put("global", USER_AVATAR_KEY, raw);

    let mut actions = vec![user_avatar()];
    if let Some(session) = session {
        if let GatewayAction::Reply(frame) = user_avatar() {
            actions.push(GatewayAction::Broadcast(crate::BroadcastFrame {
                session_id: session.to_string(),
                frame,
            }));
        }
    }
    actions
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

// --- tool groups ------------------------------------------------------------
//
// The agent owns the group table and the routing; this reads both out of the KV
// store rather than asking, and writes the pin back to override.
//
// Why the store and not a call: `available-tools` asks the agent directly, but
// it routes through `workers::call_session`, so it needs a live worker. Workers
// are the shortest-lived thing in the system, and opening a panel must not
// spawn one — a group inspector that did would be empty for exactly the stopped
// and archived conversations someone is most likely to be inspecting. The agent
// publishes its table once per turn instead.
//
// These four literals are the seam between two components that cannot share a
// constant. Their definitions, with the reasoning, are in
// `agents/agent-core/src/groups.rs`; a rename there means a grep for them here.
const TABLE_KEY: &str = "__tool_group_table";
const PIN_KEY: &str = "__tool_groups";
const WHY_KEY: &str = "__tool_groups_why";
const REASON_MANUAL: &str = "manual";


/// The published group table, or `None` before the agent has ever run a turn.
fn group_table() -> Option<Value> {
    serde_json::from_str(&sys::kv_get("global", TABLE_KEY)?).ok()
}

/// The pinned active set for a conversation. Empty means never routed, which is
/// not the same as routed to nothing — with no pin the agent offers everything.
fn pinned_groups(session_id: &str) -> Vec<String> {
    sys::kv_get(session_id, PIN_KEY)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Why each active group is active, as written during routing.
fn group_reasons(session_id: &str) -> Value {
    let mut out = serde_json::Map::new();
    for line in sys::kv_get(session_id, WHY_KEY).unwrap_or_default().lines() {
        if let Some((id, reason)) = line.split_once('=') {
            let id = id.trim();
            if !id.is_empty() {
                out.insert(id.to_string(), json!(reason.trim()));
            }
        }
    }
    Value::Object(out)
}

/// Which group a tool belongs to, according to the agent's published table.
///
/// Mirrors `groups::component_group`: a component's own `group:<id>` capability
/// wins, then the name-prefix convention, then the ungrouped fallback. Built-in
/// membership is a straight table lookup.
///
/// This replaced a second, name-derived grouping that used to live here with
/// different ids and different boundaries — so the panel grouped tools one way
/// while the agent scoped them another, and neither knew about the other. There
/// is one table now, and it is the one that decides what the model sees.
fn tool_group_id(table: &Value, name: &str, capabilities: &[String]) -> String {
    let ungrouped = table
        .get("ungrouped")
        .and_then(Value::as_str)
        .unwrap_or("extra");
    let groups = table.get("groups").and_then(Value::as_array);

    if let Some(groups) = groups {
        for group in groups {
            let members = group.get("members").and_then(Value::as_array);
            let hit = members.is_some_and(|m| m.iter().any(|v| v.as_str() == Some(name)));
            if hit {
                return group
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(ungrouped)
                    .to_string();
            }
        }
    }

    let known = |id: &str| {
        groups.is_some_and(|gs| {
            gs.iter()
                .any(|g| g.get("id").and_then(Value::as_str) == Some(id))
        })
    };

    for cap in capabilities {
        if let Some(id) = cap.strip_prefix("group:") {
            let id = id.trim();
            return if known(id) { id } else { ungrouped }.to_string();
        }
    }
    for (prefix, id) in [
        ("bq-", "bigquery"),
        ("notion-", "notion"),
        ("web-", "web"),
        ("git-", "github"),
    ] {
        if name.starts_with(prefix) && known(id) {
            return id.to_string();
        }
    }
    ungrouped.to_string()
}

/// Exactly the tools the agent would offer for this conversation's mode, each
/// tagged with its group, plus the group table and what is currently attached.
///
/// `available_tools` still answers what the agent would offer, so the tool list
/// cannot drift from reality. The group state is layered on top from the store.
fn tools(session_id: &str) -> GatewayAction {
    let table = group_table();
    let pinned = pinned_groups(session_id);
    let enabled = table
        .as_ref()
        .and_then(|t| t.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // With scoping off, or before any routing has happened, every group is
    // attached — that is exactly what the agent does with no pin.
    let all_ids: Vec<String> = table
        .as_ref()
        .and_then(|t| t.get("groups"))
        .and_then(Value::as_array)
        .map(|gs| {
            gs.iter()
                .filter_map(|g| g.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let active: Vec<String> = if !enabled || pinned.is_empty() {
        all_ids.clone()
    } else {
        pinned.clone()
    };

    // Whether the panel knows enough to claim a tool is being withheld.
    //
    // Before the agent's first turn there is no published table, so every tool
    // resolves to the ungrouped fallback and matches no active id — which the
    // naive reading turns into "0 of 75 tools attached", the exact opposite of
    // the truth, since an unrouted conversation is offered everything. The
    // fallback has to fail towards "attached": an unmarked tool that is in fact
    // withheld is a missing hint, while a tool marked withheld that is really
    // in the prompt is the panel lying about the thing it exists to report.
    let known = enabled && table.is_some();

    let tools: Vec<Value> = host::available_tools(session_id)
        .iter()
        .map(|t| {
            let group = table
                .as_ref()
                .map(|tb| tool_group_id(tb, &t.name, &t.capabilities))
                .unwrap_or_else(|| "extra".to_string());
            json!({
                "name": t.name,
                "description": t.description,
                "schema": t.args_schema_json,
                "capabilities": t.capabilities,
                "group": group,
                // Whether this tool's definition is actually in the prompt.
                // `available_tools` reports the mode-filtered surface; scoping
                // narrows it further, and that difference is the thing the
                // panel exists to show.
                "attached": !known || active.iter().any(|a| *a == group),
            })
        })
        .collect();

    reply(json!({
        "type": "tools",
        "session": session_id,
        "tools": tools,
        "groups": table.as_ref().and_then(|t| t.get("groups")).cloned().unwrap_or(json!([])),
        "active": active,
        "reasons": group_reasons(session_id),
        "grouping": enabled,
        // Distinguishes "the user has overridden or the agent has routed" from
        // "nothing has decided yet", which the active set alone cannot say.
        "routed": !pinned.is_empty(),
    }))
}

/// Whether the published table marks a group as always attached.
fn always_on(table: Option<&Value>, id: &str) -> bool {
    table
        .and_then(|t| t.get("groups"))
        .and_then(Value::as_array)
        .and_then(|gs| {
            gs.iter()
                .find(|g| g.get("id").and_then(Value::as_str) == Some(id))
        })
        .and_then(|g| g.get("always_on"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Overrides which groups this conversation offers the model.
///
/// Writes the pin the agent reads. Deliberately not validated here beyond
/// dropping unknown ids: `groups::read_pin` repairs the pin on every read —
/// forcing always-on groups back in — so the invariant that `tool_search`
/// survives is enforced by the component that depends on it, not by this one.
/// A gateway bug therefore cannot strand a conversation without an escape
/// hatch.
fn set_tool_groups(session_id: &str, frame: &Value) -> Vec<GatewayAction> {
    let Some(wanted) = frame.get("groups").and_then(Value::as_array) else {
        return vec![error("tool-groups-set requires a groups array")];
    };
    // The table is published on the agent's first turn, so a conversation that
    // has never run one has no group vocabulary to check against. That used to
    // be a hard error, which made the buttons dead on exactly the conversation
    // someone is most likely to be setting up by hand. Refuse only when there
    // is genuinely nothing to validate against.
    let table = group_table();
    let known: Vec<String> = table
        .as_ref()
        .and_then(|t| t.get("groups"))
        .and_then(Value::as_array)
        .map(|gs| {
            gs.iter()
                .filter_map(|g| g.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if known.is_empty() {
        return vec![error(
            "the agent has not published its tool groups yet — send a message first",
        )];
    }

    // Kept in table order, so the tool block the agent builds from this is
    // byte-identical whatever order the client sent, and the provider's prompt
    // cache is not missed by a reordering alone.
    //
    // Always-on groups are written in whether or not they were asked for. Two
    // reasons: the agent forces them back on read anyway, so omitting them
    // would make the panel disagree with the prompt; and an empty pin means
    // "never routed", not "routed to nothing" — so a request for no groups at
    // all would silently read as a reset and attach everything, the opposite of
    // what was asked. With the floor written, the pin is never empty.
    let chosen: Vec<String> = known
        .iter()
        .filter(|id| {
            let asked = wanted
                .iter()
                .any(|w| w.as_str().map(|w| w == id.as_str()).unwrap_or(false));
            asked || always_on(table.as_ref(), id)
        })
        .cloned()
        .collect();

    sys::kv_put(session_id, PIN_KEY, &chosen.join("\n"));

    // Reasons are rewritten wholesale: a group the user removed and re-added
    // must not still claim it arrived by tag match. Always-on groups are
    // labelled as such even though the agent will re-add them regardless, so
    // the panel can explain why they cannot be switched off.
    let why: String = chosen
        .iter()
        .map(|id| {
            let reason = if always_on(table.as_ref(), id) {
                "always-on"
            } else {
                REASON_MANUAL
            };
            format!("{id}={reason}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    sys::kv_put(session_id, WHY_KEY, &why);

    vec![tools(session_id)]
}

/// Clears the override, letting the agent route this conversation again from
/// its own evidence on the next turn.
fn reset_tool_groups(session_id: &str) -> Vec<GatewayAction> {
    sys::kv_put(session_id, PIN_KEY, "");
    sys::kv_put(session_id, WHY_KEY, "");
    vec![tools(session_id)]
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
