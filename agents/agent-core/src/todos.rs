//! A durable todo list per conversation.
//!
//! The list is conversation state, not ambient agent state: it lives in the
//! session KV store, survives compaction and restarts, and is rewritten only
//! through the functions in this module.

use crate::thetis::grip::sys;
use serde_json::{json, Value};

const TODO_KEY: &str = "__todos";
pub const MAX_ITEMS: usize = 50;
const MAX_CONTENT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl Status {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "in_progress" | "in-progress" => Ok(Self::InProgress),
            "completed" | "done" => Ok(Self::Completed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            other => Err(format!(
                "unknown todo status '{other}'; use pending, in_progress, completed, or cancelled"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
            Self::Cancelled => "[-]",
        }
    }

    fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
}

#[derive(Clone, Debug)]
pub struct Todo {
    pub id: String,
    pub content: String,
    pub active_form: String,
    pub status: Status,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug)]
pub struct TodoList {
    pub items: Vec<Todo>,
    pub next_id: u64,
    pub revision: u64,
    pub updated_ms: u64,
}

impl TodoList {
    fn empty() -> Self {
        Self { items: Vec::new(), next_id: 1, revision: 0, updated_ms: 0 }
    }

    fn to_json(&self) -> Value {
        json!({
            "items": self.items.iter().map(|item| json!({
                "id": item.id,
                "content": item.content,
                "active_form": item.active_form,
                "status": item.status.as_str(),
                "created_ms": item.created_ms,
                "updated_ms": item.updated_ms,
            })).collect::<Vec<_>>(),
            "next_id": self.next_id,
            "revision": self.revision,
            "updated_ms": self.updated_ms,
        })
    }
}

pub fn load(session_id: &str) -> TodoList {
    let Some(raw) = sys::kv_get(session_id, TODO_KEY) else { return TodoList::empty(); };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else { return TodoList::empty(); };
    let items = value.get("items").and_then(Value::as_array).map(|items| {
        items.iter().filter_map(|item| {
            let status = Status::parse(item.get("status")?.as_str()?).ok()?;
            Some(Todo {
                id: item.get("id")?.as_str()?.to_string(),
                content: item.get("content")?.as_str()?.to_string(),
                active_form: item.get("active_form").and_then(Value::as_str).unwrap_or_default().to_string(),
                status,
                created_ms: item.get("created_ms").and_then(Value::as_u64).unwrap_or(0),
                updated_ms: item.get("updated_ms").and_then(Value::as_u64).unwrap_or(0),
            })
        }).collect::<Vec<_>>()
    }).unwrap_or_default();
    TodoList {
        items,
        next_id: value.get("next_id").and_then(Value::as_u64).unwrap_or(1).max(1),
        revision: value.get("revision").and_then(Value::as_u64).unwrap_or(0),
        updated_ms: value.get("updated_ms").and_then(Value::as_u64).unwrap_or(0),
    }
}

fn store(session_id: &str, mut list: TodoList) -> Result<TodoList, String> {
    if list.items.len() > MAX_ITEMS {
        return Err(format!("a todo list may contain at most {MAX_ITEMS} items"));
    }
    list.revision += 1;
    list.updated_ms = sys::now_ms();
    sys::kv_put(session_id, TODO_KEY, &list.to_json().to_string());
    Ok(list)
}

fn item_spec(value: &Value) -> Result<(String, String, Status), String> {
    let content = value.get("content").and_then(Value::as_str)
        .ok_or_else(|| "each todo needs content".to_string())?.trim().to_string();
    if content.is_empty() { return Err("todo content must not be empty".to_string()); }
    if content.len() > MAX_CONTENT {
        return Err(format!("todo content is {} bytes, over the {MAX_CONTENT}-byte limit", content.len()));
    }
    let active = value.get("active_form").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let status = Status::parse(value.get("status").and_then(Value::as_str).unwrap_or("pending"))?;
    Ok((content, active, status))
}

/// Pure whole-list replacement, split out so id preservation is testable.
fn replace(mut existing: TodoList, specs: &[Value], now: u64) -> Result<TodoList, String> {
    if specs.len() > MAX_ITEMS { return Err(format!("a todo list may contain at most {MAX_ITEMS} items")); }
    let mut old = existing.items.clone();
    existing.items.clear();
    for spec in specs {
        let (content, active_form, status) = item_spec(spec)?;
        let prior = old.iter().position(|item| item.content == content).map(|position| old.remove(position));
        let (id, created_ms) = match prior {
            Some(item) => (item.id, item.created_ms),
            None => {
                let id = format!("t{}", existing.next_id);
                existing.next_id += 1;
                (id, now)
            }
        };
        existing.items.push(Todo { id, content, active_form, status, created_ms, updated_ms: now });
    }
    Ok(existing)
}

pub fn write(session_id: &str, specs: &[Value]) -> Result<TodoList, String> {
    let now = sys::now_ms();
    let list = replace(load(session_id), specs, now)?;
    store(session_id, list)
}

pub fn add(session_id: &str, specs: &[Value]) -> Result<TodoList, String> {
    if specs.is_empty() { return Err("give at least one todo to add".to_string()); }
    let mut list = load(session_id);
    if list.items.len() + specs.len() > MAX_ITEMS {
        return Err(format!("a todo list may contain at most {MAX_ITEMS} items"));
    }
    let now = sys::now_ms();
    for spec in specs {
        let (content, active_form, status) = item_spec(spec)?;
        let id = format!("t{}", list.next_id);
        list.next_id += 1;
        list.items.push(Todo { id, content, active_form, status, created_ms: now, updated_ms: now });
    }
    store(session_id, list)
}

pub struct Update {
    pub list: TodoList,
    pub changed: usize,
}

pub fn update(session_id: &str, updates: &[Value]) -> Result<Update, String> {
    if updates.is_empty() { return Err("give at least one todo update".to_string()); }
    let mut list = load(session_id);
    if list.items.is_empty() { return Err("there is no todo list yet — write one with todo_write".to_string()); }
    let now = sys::now_ms();
    for change in updates {
        let by_id = change.get("id").and_then(Value::as_str);
        let by_index = change.get("index").and_then(Value::as_u64);
        if by_id.is_some() == by_index.is_some() {
            return Err("each update must name exactly one of id or index".to_string());
        }
        let position = if let Some(id) = by_id {
            list.items.iter().position(|item| item.id == id).ok_or_else(|| {
                format!("no todo '{id}'; live ids: {}", list.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>().join(", "))
            })?
        } else {
            let index = by_index.unwrap() as usize;
            if index == 0 || index > list.items.len() {
                return Err(format!("todo index {index} is out of range 1..={}", list.items.len()));
            }
            index - 1
        };
        let item = &mut list.items[position];
        if let Some(status) = change.get("status").and_then(Value::as_str) { item.status = Status::parse(status)?; }
        if let Some(content) = change.get("content").and_then(Value::as_str) {
            let content = content.trim();
            if content.is_empty() || content.len() > MAX_CONTENT { return Err(format!("todo content must be 1..={MAX_CONTENT} bytes")); }
            item.content = content.to_string();
        }
        if let Some(active) = change.get("active_form").and_then(Value::as_str) { item.active_form = active.trim().to_string(); }
        item.updated_ms = now;
    }
    let changed = updates.len();
    Ok(Update { list: store(session_id, list)?, changed })
}

pub fn progress(list: &TodoList) -> (usize, usize) {
    (list.items.iter().filter(|i| i.status == Status::Completed).count(), list.items.len())
}

pub fn open_items(list: &TodoList) -> Vec<&Todo> {
    list.items.iter().filter(|i| i.status.is_open()).collect()
}

pub fn fingerprint(list: &TodoList) -> String {
    list.items.iter().map(|i| format!("{}={}", i.id, i.status.as_str())).collect::<Vec<_>>().join(";")
}

pub fn multiple_in_progress(list: &TodoList) -> Vec<&Todo> {
    list.items.iter().filter(|i| i.status == Status::InProgress).collect()
}

pub fn render(list: &TodoList) -> String {
    let (done, total) = progress(list);
    let mut out = format!("{done} of {total} done — revision {}", list.revision);
    for item in &list.items {
        out.push_str(&format!("\n{} {}: {}", item.status.mark(), item.id, item.content));
        if item.status == Status::InProgress && !item.active_form.is_empty() {
            out.push_str(&format!(" — {}", item.active_form));
        }
    }
    let active = multiple_in_progress(list);
    if active.len() > 1 {
        out.push_str(&format!("\nWarning: {} todos are in_progress; normally keep exactly one active.", active.len()));
    }
    out
}

fn nudge_for(list: &TodoList, nudges: u32, previous: &str, max_nudges: u32) -> Option<(String, String)> {
    if nudges >= max_nudges { return None; }
    let open = open_items(list);
    if open.is_empty() { return None; }
    let fingerprint = fingerprint(list);
    if fingerprint == previous { return None; }
    let names = open.iter().map(|i| format!("{} ({})", i.content, i.id)).collect::<Vec<_>>().join(", ");
    let mut text = format!(
        "The turn is ending with {} of {} todos still open: {names}. Either carry on with the next one, or — if the work is genuinely finished or no longer wanted — mark them completed or cancelled with todo_update and say briefly why. Do not re-report work you have already done.",
        open.len(), list.items.len()
    );
    if nudges + 1 == max_nudges { text.push_str(" This is the last reminder for this turn."); }
    Some((text, fingerprint))
}

pub fn stop_nudge(session_id: &str, nudges: u32, previous: &str, max_nudges: u32) -> Option<(String, String)> {
    let list = load(session_id);
    nudge_for(&list, nudges, previous, max_nudges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, content: &str, status: Status) -> Todo {
        Todo { id: id.into(), content: content.into(), active_form: String::new(), status, created_ms: 1, updated_ms: 1 }
    }
    fn list(items: Vec<Todo>) -> TodoList { TodoList { items, next_id: 9, revision: 1, updated_ms: 1 } }

    #[test]
    fn statuses_are_strict_but_accept_documented_aliases() {
        assert_eq!(Status::parse("in-progress").unwrap(), Status::InProgress);
        assert_eq!(Status::parse("done").unwrap(), Status::Completed);
        assert!(Status::parse("later").is_err());
    }

    #[test]
    fn progress_and_open_have_distinct_cancelled_semantics() {
        let l = list(vec![item("t1", "a", Status::Completed), item("t2", "b", Status::Cancelled), item("t3", "c", Status::Pending)]);
        assert_eq!(progress(&l), (1, 3));
        assert_eq!(open_items(&l).len(), 1);
    }

    #[test]
    fn fingerprint_tracks_status_not_wording() {
        let mut l = list(vec![item("t1", "a", Status::Pending)]);
        let before = fingerprint(&l);
        l.items[0].content = "renamed".into();
        assert_eq!(before, fingerprint(&l));
        l.items[0].status = Status::Completed;
        assert_ne!(before, fingerprint(&l));
    }

    #[test]
    fn render_distinguishes_all_statuses() {
        let l = list(vec![item("t1", "a", Status::Pending), item("t2", "b", Status::InProgress), item("t3", "c", Status::Completed), item("t4", "d", Status::Cancelled)]);
        let shown = render(&l);
        for mark in ["[ ]", "[~]", "[x]", "[-]"] { assert!(shown.contains(mark)); }
    }

    #[test]
    fn nudge_decision_is_bounded_and_requires_changed_open_work() {
        let empty = list(vec![]);
        assert!(nudge_for(&empty, 0, "", 2).is_none());
        let closed = list(vec![item("t1", "a", Status::Completed), item("t2", "b", Status::Cancelled)]);
        assert!(nudge_for(&closed, 0, "", 2).is_none());
        let open = list(vec![item("t1", "a", Status::Pending)]);
        let (_, fingerprint) = nudge_for(&open, 0, "", 2).expect("open work nudges");
        assert!(nudge_for(&open, 1, &fingerprint, 2).is_none());
        assert!(nudge_for(&open, 2, "", 2).is_none());
    }

    #[test]
    fn replacing_preserves_a_matching_items_identity() {
        let l = list(vec![item("t4", "keep", Status::Pending)]);
        let next = replace(l, &[json!({"content":"keep", "status":"completed"})], 7).unwrap();
        assert_eq!(next.items[0].id, "t4");
        assert_eq!(next.items[0].created_ms, 1);
    }
}
