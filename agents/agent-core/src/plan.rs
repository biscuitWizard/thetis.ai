//! The plan document: one durable, editable artefact per conversation.
//!
//! Plan mode used to be ascetic — it withheld every mutating tool, appended a
//! prompt asking for a plan, and left the plan itself as prose in the
//! transcript. That made the plan the one output of the mode with no home: it
//! could not be revised without restating it, it scrolled away under the
//! investigation that produced it, and "switch to Agent mode and carry it out"
//! meant the model re-derived from a transcript rather than reading a document.
//!
//! So a plan is a document here, held in the session KV store, addressed by
//! search string the way code is. Three properties follow from that choice and
//! are worth keeping:
//!
//!  1. **Editable in place.** `edit` takes the exact text to replace, refuses an
//!     ambiguous match, and never rewrites what it was not asked to. This is the
//!     same contract as `edit_path`, deliberately: a plan under revision has the
//!     same failure mode as code under revision — a well-meant rewrite that
//!     quietly drops a section the reader had already approved.
//!  2. **Writable from a read-only mode.** Every function here is non-mutating
//!     in the sense the mode filter means: it changes this conversation's own
//!     notes and nothing outside it. `remember` is declared the same way for the
//!     same reason. A plan mode that could not write its plan would be the bug
//!     this module exists to fix.
//!  3. **Nothing is lost silently.** Every write bumps a revision and stamps a
//!     time, so the surface can say the plan moved and the reader can tell a
//!     stale tab from a current one.
//!
//! The key is `__`-prefixed, which `remember` refuses, so a note can never
//! collide with the plan.

use crate::thetis::grip::sys;
use serde_json::{json, Value};

/// Where the plan lives, in the session's own KV scope.
const PLAN_KEY: &str = "__plan";

/// Ceiling on the stored document, in bytes.
///
/// A plan is a document a person reads and a model is expected to hold in
/// context alongside everything else; past this size it has stopped being a plan
/// and become the implementation. Refusing is kinder than truncating, which
/// would drop the end — the part most recently written and least likely to be
/// remembered.
const MAX_BODY: usize = 200_000;

/// A plan as stored. Absent fields read as empty, so a document written by an
/// older revision of this module still loads.
pub struct Plan {
    pub title: String,
    pub body: String,
    pub updated_ms: u64,
    pub revision: u64,
}

impl Plan {
    fn empty() -> Self {
        Plan { title: String::new(), body: String::new(), updated_ms: 0, revision: 0 }
    }

    fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "body": self.body,
            "updated_ms": self.updated_ms,
            "revision": self.revision,
        })
    }
}

/// Reads the plan, or an empty one.
///
/// An unparseable record reads as empty rather than as an error: the only writer
/// is this module, so a bad record means a format that no longer exists, and
/// failing every call thereafter would strand the conversation with no way to
/// write a new plan over the top.
pub fn load(session_id: &str) -> Plan {
    let Some(raw) = sys::kv_get(session_id, PLAN_KEY) else {
        return Plan::empty();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Plan::empty();
    };
    Plan {
        title: value.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
        body: value.get("body").and_then(Value::as_str).unwrap_or_default().to_string(),
        updated_ms: value.get("updated_ms").and_then(Value::as_u64).unwrap_or(0),
        revision: value.get("revision").and_then(Value::as_u64).unwrap_or(0),
    }
}

fn store(session_id: &str, mut plan: Plan) -> Result<Plan, String> {
    if plan.body.len() > MAX_BODY {
        return Err(format!(
            "that plan is {} bytes, over the {MAX_BODY}-byte limit. A plan this long is an \
             implementation — split it, or keep the detail in files and reference them.",
            plan.body.len()
        ));
    }
    plan.revision += 1;
    plan.updated_ms = sys::now_ms();
    sys::kv_put(session_id, PLAN_KEY, &plan.to_json().to_string());
    Ok(plan)
}

/// Replaces the plan outright.
pub fn write(session_id: &str, title: Option<String>, body: &str) -> Result<Plan, String> {
    if body.trim().is_empty() {
        return Err("a plan needs a body. To clear one, write a short note saying why.".to_string());
    }
    let existing = load(session_id);
    store(
        session_id,
        Plan {
            // A write that says nothing about the title keeps the one already
            // there: retitling and rewriting are different intentions, and
            // blanking the title as a side effect of revising the body is the
            // kind of quiet loss this module is arranged to avoid.
            title: title.unwrap_or(existing.title),
            body: body.to_string(),
            revision: existing.revision,
            updated_ms: existing.updated_ms,
        },
    )
}

/// Adds to the end of the plan.
///
/// Exists so that growing a plan by a section does not require restating the
/// whole of it. A read-modify-write through `write` would work, and would also
/// be the commonest way for a long plan to lose a paragraph.
pub fn append(session_id: &str, text: &str) -> Result<Plan, String> {
    if text.trim().is_empty() {
        return Err("nothing to append".to_string());
    }
    let plan = load(session_id);
    if plan.body.trim().is_empty() {
        return write(session_id, None, text);
    }
    let joined = format!("{}\n\n{}", plan.body.trim_end(), text.trim_start());
    store(session_id, Plan { body: joined, ..plan })
}

/// The outcome of an edit, so the caller can report how many places moved.
pub struct Edit {
    pub plan: Plan,
    pub replacements: usize,
}

/// Replaces an exact snippet, the way `edit_path` does.
///
/// The two refusals are the whole value of the function. A snippet that is not
/// found means the model is editing a plan it has not read — usually a plan
/// another turn has since revised — and applying nothing while reporting success
/// would leave it building on a document that does not exist. A snippet that
/// appears more than once means the intended site is genuinely ambiguous, and
/// guessing the first is how the wrong step gets rewritten.
pub fn edit(
    session_id: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<Edit, String> {
    if old_text.is_empty() {
        return Err("old_text must not be empty; use plan_write to replace the whole plan".to_string());
    }
    let plan = load(session_id);
    if plan.body.trim().is_empty() {
        return Err("there is no plan yet — write one with plan_write first".to_string());
    }

    let hits = plan.body.matches(old_text).count();
    if hits == 0 {
        return Err(format!(
            "that text is not in the plan. Read it back with plan_read: it must match byte for \
             byte, newlines and indentation included.{}",
            near_miss_hint(&plan.body, old_text)
        ));
    }
    if hits > 1 && !replace_all {
        return Err(format!(
            "that text appears {hits} times in the plan, so the edit is ambiguous. Include \
             surrounding lines until it is unique, or pass replace_all to change every one."
        ));
    }

    let body = if replace_all {
        plan.body.replace(old_text, new_text)
    } else {
        plan.body.replacen(old_text, new_text, 1)
    };
    let replacements = if replace_all { hits } else { 1 };
    Ok(Edit { plan: store(session_id, Plan { body, ..plan })?, replacements })
}

/// A hint when an exact match failed but a near one exists.
///
/// The commonest cause by far is whitespace: a model reproducing a bullet from
/// memory gets the words right and the leading spaces or the line breaks wrong.
/// Saying so turns a retry loop into one corrected call.
fn near_miss_hint(body: &str, old_text: &str) -> String {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat_body = squash(body);
    let flat_needle = squash(old_text);
    if !flat_needle.is_empty() && flat_body.contains(&flat_needle) {
        return " The words do occur, but the whitespace differs — copy the line from plan_read \
                rather than retyping it."
            .to_string();
    }
    String::new()
}

/// The plan rendered for a tool result: the document, plus where it now stands.
pub fn describe(plan: &Plan) -> String {
    let title = if plan.title.trim().is_empty() { "(untitled)" } else { plan.title.trim() };
    format!(
        "plan '{title}' — revision {}, {} lines\n\n{}",
        plan.revision,
        plan.body.lines().count(),
        plan.body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The near-miss hint fires on the case it exists for — same words, wrong
    /// whitespace — and stays quiet when the text is genuinely absent, where it
    /// would only be misleading.
    #[test]
    fn a_whitespace_only_difference_is_called_out() {
        let body = "- step one\n  - detail\n- step two";
        assert!(!near_miss_hint(body, "- step one\n- detail").is_empty());
        assert!(near_miss_hint(body, "- step three").is_empty());
    }

    #[test]
    fn the_hint_is_quiet_for_an_empty_needle() {
        assert!(near_miss_hint("anything", "").is_empty());
    }
}
