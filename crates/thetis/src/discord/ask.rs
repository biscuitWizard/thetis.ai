//! Rendering the `ask_user` tool call as Discord components, and reading the
//! answers back.
//!
//! ## Why one question per message
//!
//! Discord allows at most five action rows on a message, and a select menu
//! takes a whole row. A form of five questions plus a submit row therefore does
//! not fit, and the arithmetic gets worse as soon as a question needs both a
//! menu and a button. Asking one question at a time sidesteps the limit
//! entirely, and it reads the way a conversation reads: a question, an answer,
//! the next question. The cost is more messages, which chat is made of anyway.
//!
//! ## Why free text needs a modal
//!
//! Discord has no text input on a message — text inputs are legal only inside a
//! modal. So "type your own answer" cannot be a box under the options; it has to
//! be a control that opens one. That is why every choice question here carries a
//! menu entry *and* the interaction that follows opens a modal, rather than the
//! web UI's inline field.
//!
//! ## Where the state lives
//!
//! In the KV store, not in memory. A form can sit unanswered for hours, and the
//! orchestrator restarts whenever a branch is merged; state held in a task would
//! leave a message whose buttons answer nothing. The `custom_id` carries the
//! state's key, because on a modal submission it is the only routing information
//! Discord gives back.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// KV scope for form state. Global because a form belongs to a channel, and
/// channels are not sessions.
pub const SCOPE: &str = "global";

/// How long a posted form is answerable. Beyond this the state is treated as
/// gone: the questions belonged to a turn that has long since ended, and
/// answering them would drop an answer into an unrelated conversation.
pub const TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Discord's own ceilings. Exceeding any one of them fails the whole message,
/// so every string that reaches Discord is cut to fit rather than trusted.
const LABEL_MAX: usize = 100;
const BUTTON_LABEL_MAX: usize = 80;
const PLACEHOLDER_MAX: usize = 150;
const MODAL_LABEL_MAX: usize = 45;
/// A select menu holds 25 options. Two are reserved for the free-text entry and
/// the skip entry, which every question must offer.
const MENU_OPTIONS_MAX: usize = 25;
const RESERVED_ENTRIES: usize = 2;
pub const OPTIONS_MAX: usize = MENU_OPTIONS_MAX - RESERVED_ENTRIES;

/// Buttons rather than a menu below this many options: a two-way choice reads
/// better as two buttons than as a dropdown hiding both answers.
const BUTTON_THRESHOLD: usize = 2;

/// The label of the free-text escape hatch, on every surface.
pub const OTHER_LABEL: &str = "Something else…";

/// Sentinel option values. Prefixed so they cannot collide with an index.
const VALUE_OTHER: &str = "other";
const VALUE_SKIP: &str = "skip";

/// Cuts a string to a character budget, marking it when it had to be cut.
///
/// Character counts, not bytes: Discord counts characters, and cutting a
/// multi-byte string by bytes both miscounts and panics on a boundary.
fn fit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    text.chars().take(keep).collect::<String>() + "…"
}

// --- the questions ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Choice,
    Open,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub prompt: String,
    pub kind: Kind,
    pub options: Vec<String>,
    pub multiple: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ask {
    pub intro: String,
    pub questions: Vec<Question>,
}

/// Reads a tool call's `arguments_json` into questions.
///
/// Returns `None` when there is nothing askable, so the caller can fall back to
/// its ordinary rendering instead of posting an empty form. The kind is inferred
/// exactly as the tool infers it, so a call that omitted `type` renders as the
/// tool described it.
pub fn parse(arguments_json: &str) -> Option<Ask> {
    let value: Value = serde_json::from_str(arguments_json).ok()?;
    let raw = value.get("questions")?.as_array()?;

    let questions: Vec<Question> = raw
        .iter()
        .filter_map(|q| {
            let prompt = q.get("question")?.as_str()?.trim().to_string();
            if prompt.is_empty() {
                return None;
            }
            let options: Vec<String> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .take(OPTIONS_MAX)
                        .collect()
                })
                .unwrap_or_default();
            let kind = match q.get("type").and_then(Value::as_str) {
                Some("choice") => Kind::Choice,
                Some("open") => Kind::Open,
                _ if options.is_empty() => Kind::Open,
                _ => Kind::Choice,
            };
            Some(Question {
                prompt,
                kind,
                // An open question's options are meaningless; dropping them here
                // keeps every later stage from having to ask which kind it has.
                options: if kind == Kind::Choice {
                    options
                } else {
                    Vec::new()
                },
                multiple: q
                    .get("allow_multiple")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect();

    if questions.is_empty() {
        return None;
    }
    Some(Ask {
        intro: value
            .get("intro")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        questions,
    })
}

// --- the answers -----------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Answer {
    pub skipped: bool,
    /// What was chosen or typed. Several selections are joined with ", " so the
    /// model reads one answer per question however it was collected.
    pub text: String,
}

/// One posted form, waiting on the person it was addressed to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// The Thetis conversation the answers are submitted to.
    pub session_id: String,
    pub channel_id: String,
    /// Who may answer. A form posted in a shared channel is addressed to the
    /// person whose turn produced it, and letting a bystander answer would put
    /// words in their mouth.
    pub user_id: String,
    pub ask: Ask,
    pub answers: Vec<Answer>,
    /// The question being asked now. Equal to `answers.len()` once done.
    pub index: usize,
    /// When the form was posted, in epoch milliseconds.
    pub created_ms: i64,
    /// The message carrying the controls, once it is posted.
    ///
    /// Needed so a form that is superseded can have its controls taken away
    /// rather than left clickable: a stale form nobody retired is a second
    /// answer stream into the same conversation. `default` so state written by
    /// an earlier build still loads instead of stranding a live form.
    #[serde(default)]
    pub message_id: Option<String>,
}

impl State {
    pub fn new(session_id: &str, channel_id: &str, user_id: &str, ask: Ask, now_ms: i64) -> Self {
        Self {
            session_id: session_id.to_string(),
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            ask,
            answers: Vec::new(),
            index: 0,
            created_ms: now_ms,
            message_id: None,
        }
    }

    pub fn current(&self) -> Option<&Question> {
        self.ask.questions.get(self.index)
    }

    pub fn done(&self) -> bool {
        self.index >= self.ask.questions.len()
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.created_ms) > TTL_MS
    }

    /// Records an answer and moves on.
    pub fn record(&mut self, answer: Answer) {
        while self.answers.len() < self.index {
            self.answers.push(Answer {
                skipped: true,
                text: String::new(),
            });
        }
        self.answers.push(answer);
        self.index += 1;
    }
}

/// The KV key holding a form's state.
pub fn key(state_id: &str) -> String {
    format!("discord.ask.{state_id}")
}

/// The KV key naming the form posted for one `ask_user` call.
///
/// A tool call is the unit of asking, so it is the unit of claiming: whoever
/// writes this key first owns posting the form, and everyone else finds it
/// taken. Several readers of the event stream can see the same
/// `tool-invocation` — a message arriving mid-turn starts a second follower of
/// the same session, and a reconnect can replay one — and without a claim each
/// of them posts its own form. Two live forms for one question is the fork:
/// both are answerable, and both submit, so the model is handed two answers to
/// a question it asked once.
///
/// Never cleared. A few dozen bytes per call buys idempotence that survives a
/// restart, which a key expiring with the form would not.
pub fn claim_key(session_id: &str, call_id: &str) -> String {
    format!("discord.ask.call.{session_id}.{call_id}")
}

/// The KV key naming the one form a session may currently have outstanding.
///
/// A session answers into a single conversation, so it may have at most one
/// answerable form. Per-call claiming alone does not give that: two *different*
/// calls — a second turn asking again before the first form was dealt with —
/// each claim their own key and each post, leaving two sets of controls live in
/// the channel. Both are answerable and both submit, so one conversation
/// receives two independent answer streams, which is the fork.
///
/// Holds the state id of the live form, or empty for none, so posting a new
/// form can retire the previous one in the same compare-and-set that installs
/// it.
pub fn live_key(session_id: &str) -> String {
    format!("discord.ask.live.{session_id}")
}

/// A short stable id for a string, for a call that arrives without one.
///
/// FNV-1a, in base36. Not a secret and nothing depends on its strength; it only
/// has to give the same answer twice, so two readers seeing the same call agree
/// on the same claim key. A provider that omitted `id` would otherwise leave
/// every reader free to claim separately, which is the fork this prevents.
pub fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = String::new();
    let mut v = hash;
    for _ in 0..12 {
        out.push(char::from_digit((v % 36) as u32, 36).unwrap_or('0'));
        v /= 36;
    }
    out
}

// --- custom ids ------------------------------------------------------------

/// What a component press means. Encoded into the `custom_id`, because that is
/// the only thing Discord hands back on a modal submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A select menu or a button carrying one option's value.
    Choose,
    /// Open the free-text modal.
    Other,
    Skip,
    /// The modal came back with typed text.
    Typed,
}

impl Action {
    fn tag(&self) -> &'static str {
        match self {
            Action::Choose => "c",
            Action::Other => "o",
            Action::Skip => "s",
            Action::Typed => "t",
        }
    }
}

/// What a `custom_id` decoded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub state_id: String,
    /// Which question the control belonged to when it was drawn.
    pub index: usize,
    pub action: Action,
    /// A button's option index. A select menu sends its choice in `values`
    /// instead, and a button has no `values` at all — so a one-click answer has
    /// nowhere to put the choice except the id.
    pub option: Option<usize>,
}

/// `ask:<state id>:<question index>:<action>[:<option index>]`.
///
/// The question index is part of the id so a stale message — one left on screen
/// from an earlier question — cannot answer the question now in front of the
/// user. Discord caps a `custom_id` at 100 characters, which this stays inside
/// as long as the state id is short.
pub fn custom_id(state_id: &str, index: usize, action: Action) -> String {
    format!("ask:{state_id}:{index}:{}", action.tag())
}

/// A button's id: the same shape with the chosen option's index appended.
///
/// The index rather than the label, because a label may be far longer than a
/// `custom_id` is allowed to be.
pub fn button_id(state_id: &str, index: usize, option: usize) -> String {
    format!("{}:{option}", custom_id(state_id, index, Action::Choose))
}

/// Reads a `custom_id` back. `None` for anything that is not one of ours.
pub fn parse_custom_id(raw: &str) -> Option<Route> {
    let mut parts = raw.split(':');
    if parts.next()? != "ask" {
        return None;
    }
    let state_id = parts.next()?.to_string();
    let index = parts.next()?.parse::<usize>().ok()?;
    let action = match parts.next()? {
        "c" => Action::Choose,
        "o" => Action::Other,
        "s" => Action::Skip,
        "t" => Action::Typed,
        _ => return None,
    };
    let option = match parts.next() {
        Some(tail) => Some(tail.parse::<usize>().ok()?),
        None => None,
    };
    // Anything further is not a shape this build writes, so it is not ours.
    if parts.next().is_some() {
        return None;
    }
    Some(Route {
        state_id,
        index,
        action,
        option,
    })
}

// --- rendering -------------------------------------------------------------

/// The text above the controls: what has been answered, and what is being asked.
pub fn prompt_text(state: &State) -> String {
    let total = state.ask.questions.len();
    let mut out = String::new();

    if state.index == 0 && !state.ask.intro.is_empty() {
        out.push_str(&format!("_{}_\n\n", state.ask.intro));
    }
    // Answers so far are restated rather than left in scrollback: the message is
    // edited in place, so without this the record of what was said disappears.
    for (i, answer) in state.answers.iter().enumerate() {
        let question = state
            .ask
            .questions
            .get(i)
            .map(|q| q.prompt.as_str())
            .unwrap_or("");
        let shown = if answer.skipped {
            "_skipped_".to_string()
        } else {
            answer.text.clone()
        };
        out.push_str(&format!("~~{}~~ → **{}**\n", question, shown));
    }

    match state.current() {
        Some(question) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "**Question {} of {}**\n{}",
                state.index + 1,
                total,
                question.prompt
            ));
            if question.multiple && question.kind == Kind::Choice {
                out.push_str("\n_Pick as many as apply._");
            }
        }
        None => {
            out.push_str("\nAll answered — sending.");
        }
    }
    out
}

/// The action rows for the question now being asked.
///
/// Empty when the form is finished, which is also what retires the controls: an
/// empty component array is how Discord is told to take them away.
pub fn components(state: &State, state_id: &str) -> Value {
    let Some(question) = state.current() else {
        return json!([]);
    };
    let index = state.index;
    let mut rows = Vec::new();

    match question.kind {
        // An open question has nothing to choose from, so the free-text control
        // is the whole of it — and it must be a button, because Discord has no
        // text input outside a modal.
        Kind::Open => {}
        Kind::Choice if question.options.len() <= BUTTON_THRESHOLD => {
            // Few enough to show outright. Two visible answers beat a dropdown
            // that hides both.
            let buttons: Vec<Value> = question
                .options
                .iter()
                .enumerate()
                .map(|(i, option)| {
                    json!({
                        "type": 2,
                        "style": 2,
                        "label": fit(option, BUTTON_LABEL_MAX),
                        "custom_id": button_id(state_id, index, i),
                    })
                })
                .collect();
            if !buttons.is_empty() {
                rows.push(json!({ "type": 1, "components": buttons }));
            }
        }
        Kind::Choice => {
            let mut options: Vec<Value> = question
                .options
                .iter()
                .enumerate()
                .map(|(i, option)| {
                    json!({
                        "label": fit(option, LABEL_MAX),
                        "value": i.to_string(),
                    })
                })
                .collect();
            // Always last, always present: the option list is the agent's guess
            // at the answer space, and this is how someone disagrees with it.
            options.push(json!({
                "label": OTHER_LABEL,
                "value": VALUE_OTHER,
                "description": fit("Type your own answer instead", LABEL_MAX),
            }));
            options.push(json!({
                "label": "Skip this question",
                "value": VALUE_SKIP,
            }));

            let max_values = if question.multiple {
                question.options.len().max(1)
            } else {
                1
            };
            rows.push(json!({
                "type": 1,
                "components": [{
                    "type": 3,
                    "custom_id": custom_id(state_id, index, Action::Choose),
                    "placeholder": fit(&question.prompt, PLACEHOLDER_MAX),
                    "min_values": 1,
                    "max_values": max_values,
                    "options": options,
                }],
            }));
        }
    }

    // Every question can be escaped and every question can be skipped, on every
    // path. These two are added here rather than by the caller so no shape of
    // question can accidentally omit them.
    let mut tail = vec![json!({
        "type": 2,
        "style": 2,
        "label": if question.kind == Kind::Open { "Answer" } else { "Type my own answer" },
        "custom_id": custom_id(state_id, index, Action::Other),
    })];
    tail.push(json!({
        "type": 2,
        "style": 2,
        "label": "Skip",
        "custom_id": custom_id(state_id, index, Action::Skip),
    }));
    rows.push(json!({ "type": 1, "components": tail }));

    json!(rows)
}

/// The modal that collects a free-text answer.
pub fn modal(state: &State, state_id: &str) -> Value {
    let prompt = state
        .current()
        .map(|q| q.prompt.as_str())
        .unwrap_or("Your answer");
    json!({
        "custom_id": custom_id(state_id, state.index, Action::Typed),
        "title": fit(prompt, MODAL_LABEL_MAX),
        "components": [{
            "type": 1,
            "components": [{
                "type": 4,
                "custom_id": "answer",
                // Discord caps a label at 45 characters, which is shorter than
                // most questions — so the question goes in the message above and
                // the label just says what the box is.
                "label": fit("Your answer", MODAL_LABEL_MAX),
                "style": 2,
                "required": true,
                "max_length": 1000,
            }],
        }],
    })
}

/// Turns a select menu's values into an answer, or `None` for "open the modal".
///
/// `None` is not a failure: choosing the free-text entry is a legitimate outcome
/// that the caller answers with a modal rather than a recorded answer.
pub fn answer_from_values(question: &Question, values: &[String]) -> Option<Answer> {
    if values.iter().any(|v| v == VALUE_OTHER) {
        return None;
    }
    if values.iter().any(|v| v == VALUE_SKIP) {
        return Some(Answer {
            skipped: true,
            text: String::new(),
        });
    }
    let chosen: Vec<String> = values
        .iter()
        .filter_map(|v| v.parse::<usize>().ok())
        .filter_map(|i| question.options.get(i).cloned())
        .collect();
    if chosen.is_empty() {
        // A selection that resolved to nothing is a stale message answering a
        // question whose options have changed. Skipping is the honest reading:
        // no option here was actually chosen.
        return Some(Answer {
            skipped: true,
            text: String::new(),
        });
    }
    Some(Answer {
        skipped: false,
        text: chosen.join(", "),
    })
}

/// An answer from a button press: the option its id named.
pub fn answer_from_option(question: &Question, option: usize) -> Answer {
    match question.options.get(option) {
        Some(text) => Answer {
            skipped: false,
            text: text.clone(),
        },
        // A press on a row whose options no longer exist. Reading it as a skip
        // is the honest outcome; inventing an answer is not.
        None => Answer {
            skipped: true,
            text: String::new(),
        },
    }
}

/// The message the answers become.
///
/// Prose with the questions restated, because the model reads this as an
/// ordinary user message: it cannot see the form, so an answer that does not
/// name its question is one it has to guess at.
pub fn compose(state: &State) -> String {
    let mut lines = Vec::new();
    for (i, question) in state.ask.questions.iter().enumerate() {
        let shown = match state.answers.get(i) {
            Some(a) if a.skipped => "(skipped)".to_string(),
            Some(a) if a.text.trim().is_empty() => "(no answer)".to_string(),
            Some(a) => a.text.clone(),
            None => "(no answer)".to_string(),
        };
        lines.push(format!("{}. {}\n   {}", i + 1, question.prompt, shown));
    }
    format!("Answers:\n\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask_of(json_text: &str) -> Ask {
        parse(json_text).expect("should parse")
    }

    #[test]
    fn a_call_with_no_questions_renders_nothing() {
        assert!(parse("{}").is_none());
        assert!(parse(r#"{"questions":[]}"#).is_none());
        assert!(parse("not json").is_none());
        // A question with no prompt is not a question.
        assert!(parse(r#"{"questions":[{"question":"  "}]}"#).is_none());
    }

    #[test]
    fn the_kind_is_inferred_from_the_options_exactly_as_the_tool_infers_it() {
        let ask = ask_of(
            r#"{"questions":[
                {"question":"open one"},
                {"question":"choice one","options":["a","b"]},
                {"question":"forced open","options":["a"],"type":"open"}
            ]}"#,
        );
        assert_eq!(ask.questions[0].kind, Kind::Open);
        assert_eq!(ask.questions[1].kind, Kind::Choice);
        assert_eq!(ask.questions[2].kind, Kind::Open);
        // An open question's options are dropped, so no later stage has to ask.
        assert!(ask.questions[2].options.is_empty());
    }

    #[test]
    fn every_choice_question_offers_free_text_and_a_skip() {
        // The guarantee the user asked for, checked on both renderings: a menu
        // for a long list, buttons for a short one.
        for count in [2usize, 6] {
            let options: Vec<String> = (0..count).map(|i| format!("\"o{i}\"")).collect();
            let ask = ask_of(&format!(
                r#"{{"questions":[{{"question":"q","options":[{}]}}]}}"#,
                options.join(",")
            ));
            let state = State::new("s", "c", "u", ask, 0);
            let rows = components(&state, "abc");
            let text = rows.to_string();
            assert!(
                text.contains(OTHER_LABEL) || text.contains("Type my own answer"),
                "{count} options must offer free text: {text}"
            );
            assert!(text.contains("Skip"), "{count} options must offer a skip");
        }
    }

    #[test]
    fn an_open_question_still_gets_both_controls() {
        let state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"q"}]}"#),
            0,
        );
        let text = components(&state, "abc").to_string();
        assert!(text.contains("Answer"));
        assert!(text.contains("Skip"));
    }

    #[test]
    fn the_controls_retire_when_the_form_is_finished() {
        let mut state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"q"}]}"#),
            0,
        );
        state.record(Answer {
            skipped: false,
            text: "yes".into(),
        });
        assert!(state.done());
        // An empty array is what Discord reads as "take the components away".
        assert_eq!(components(&state, "abc"), json!([]));
    }

    #[test]
    fn a_custom_id_round_trips_and_rejects_anything_else() {
        let route = parse_custom_id(&custom_id("s1", 3, Action::Skip)).unwrap();
        assert_eq!(route.state_id, "s1");
        assert_eq!(route.index, 3);
        assert_eq!(route.action, Action::Skip);
        assert_eq!(route.option, None);
        // A button carries its option in the tail.
        let pressed = parse_custom_id(&button_id("s1", 3, 7)).unwrap();
        assert_eq!(pressed.action, Action::Choose);
        assert_eq!(pressed.option, Some(7));

        assert!(parse_custom_id("something:else:3:s").is_none());
        assert!(parse_custom_id("ask:s1:notanumber:s").is_none());
        assert!(parse_custom_id("ask:s1:3:z").is_none());
        assert!(parse_custom_id("ask:s1:3").is_none());
        assert!(parse_custom_id("ask:s1:3:c:notanumber").is_none());
    }

    #[test]
    fn a_custom_id_fits_discords_hundred_character_limit() {
        // The state id is ours to keep short, so this is a real bound rather
        // than a hope: a longer id would fail the whole message.
        let id = button_id("abcdefgh", 4, 24);
        assert!(id.len() <= 100, "custom_id too long: {} chars", id.len());
    }

    #[test]
    fn a_stale_message_cannot_answer_the_current_question() {
        // The index is in the id, so a press on an earlier question's row is
        // identifiable rather than being applied to whatever is being asked now.
        let index = parse_custom_id(&custom_id("s1", 0, Action::Choose))
            .unwrap()
            .index;
        assert_eq!(index, 0);
        let mut state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"a"},{"question":"b"}]}"#),
            0,
        );
        state.record(Answer::default());
        assert_ne!(
            index, state.index,
            "an old row must not match the new index"
        );
    }

    #[test]
    fn selecting_free_text_asks_for_a_modal_rather_than_recording_an_answer() {
        let ask = ask_of(r#"{"questions":[{"question":"q","options":["a","b","c"]}]}"#);
        let q = &ask.questions[0];
        assert!(answer_from_values(q, &[VALUE_OTHER.to_string()]).is_none());
        let skipped = answer_from_values(q, &[VALUE_SKIP.to_string()]).unwrap();
        assert!(skipped.skipped);
        let chosen = answer_from_values(q, &["1".to_string()]).unwrap();
        assert_eq!(chosen.text, "b");
    }

    #[test]
    fn several_selections_become_one_answer() {
        let ask = ask_of(
            r#"{"questions":[{"question":"q","options":["a","b","c"],"allow_multiple":true}]}"#,
        );
        let answer = answer_from_values(&ask.questions[0], &["0".into(), "2".into()]).unwrap();
        assert_eq!(answer.text, "a, c");
    }

    #[test]
    fn an_unresolvable_selection_is_read_as_a_skip_not_as_an_empty_answer() {
        let ask = ask_of(r#"{"questions":[{"question":"q","options":["a"]}]}"#);
        let answer = answer_from_values(&ask.questions[0], &["99".into()]).unwrap();
        assert!(answer.skipped);
    }

    #[test]
    fn the_option_list_is_capped_so_a_menu_cannot_overflow() {
        let options: Vec<String> = (0..40).map(|i| format!("\"o{i}\"")).collect();
        let ask = ask_of(&format!(
            r#"{{"questions":[{{"question":"q","options":[{}]}}]}}"#,
            options.join(",")
        ));
        assert_eq!(ask.questions[0].options.len(), OPTIONS_MAX);
        // Plus the two reserved entries, which is exactly Discord's ceiling.
        assert_eq!(OPTIONS_MAX + RESERVED_ENTRIES, MENU_OPTIONS_MAX);
    }

    #[test]
    fn long_labels_are_cut_on_a_character_boundary() {
        let wide = "é".repeat(200);
        let ask = ask_of(&format!(
            r#"{{"questions":[{{"question":"q","options":["{wide}","b","c"]}}]}}"#
        ));
        let state = State::new("s", "c", "u", ask, 0);
        let rows = components(&state, "abc");
        let label = rows[0]["components"][0]["options"][0]["label"]
            .as_str()
            .unwrap();
        assert_eq!(label.chars().count(), LABEL_MAX);
    }

    #[test]
    fn the_composed_message_names_every_question_including_the_unanswered() {
        let mut state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"first"},{"question":"second"}]}"#),
            0,
        );
        state.record(Answer {
            skipped: false,
            text: "yes".into(),
        });
        state.record(Answer {
            skipped: true,
            text: String::new(),
        });
        let text = compose(&state);
        assert!(text.contains("1. first\n   yes"), "{text}");
        assert!(text.contains("2. second\n   (skipped)"), "{text}");
    }

    #[test]
    fn a_form_expires_so_an_old_message_cannot_answer_a_finished_turn() {
        let state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"q"}]}"#),
            0,
        );
        assert!(!state.expired(TTL_MS));
        assert!(state.expired(TTL_MS + 1));
    }

    #[test]
    fn answered_questions_stay_visible_in_the_edited_message() {
        // The message is edited in place, so without restating them the record
        // of what was answered would vanish as the form advances.
        let mut state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"first"},{"question":"second"}]}"#),
            0,
        );
        state.record(Answer {
            skipped: false,
            text: "yes".into(),
        });
        let text = prompt_text(&state);
        assert!(text.contains("first"), "{text}");
        assert!(text.contains("yes"), "{text}");
        assert!(text.contains("Question 2 of 2"), "{text}");
    }

    #[test]
    fn a_call_is_claimed_by_its_own_key_and_nothing_elses() {
        // The claim key is what makes posting idempotent per call, so it must
        // distinguish calls and sessions and nothing else.
        let a = claim_key("sess-1", "call-1");
        assert_eq!(claim_key("sess-1", "call-1"), a, "must be stable");
        assert_ne!(claim_key("sess-1", "call-2"), a);
        assert_ne!(claim_key("sess-2", "call-1"), a);
        // Distinct from the form-state key, or claiming would destroy the state.
        assert_ne!(a, key("call-1"));
    }

    #[test]
    fn the_digest_is_stable_and_distinguishes_different_calls() {
        // Stands in for a missing call id, so two readers of the same event must
        // land on the same claim, and two different calls must not collide.
        let args = r#"{"questions":[{"question":"q"}]}"#;
        assert_eq!(digest(args), digest(args));
        assert_ne!(digest(args), digest(r#"{"questions":[{"question":"r"}]}"#));
        assert!(!digest(args).is_empty());
        // Short enough to sit inside a 100-character custom_id alongside the
        // rest of the key.
        assert!(digest(args).chars().count() <= 12);
        assert!(digest(args).chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn a_session_has_one_live_form_pointer_distinct_from_call_and_state_keys() {
        // At most one form per session may be answerable, and the pointer that
        // enforces it must not collide with the per-call claim or the form
        // state — either collision would destroy the thing it names.
        let live = live_key("sess-1");
        assert_eq!(live_key("sess-1"), live);
        assert_ne!(live_key("sess-2"), live);
        assert_ne!(live, claim_key("sess-1", "call-1"));
        assert_ne!(live, key("sess-1"));
    }

    #[test]
    fn state_from_a_build_without_a_message_id_still_loads() {
        // A form outliving a deploy must stay answerable, so the new field is
        // defaulted rather than required.
        let old = r#"{"session_id":"s","channel_id":"c","user_id":"u",
            "ask":{"intro":"","questions":[{"prompt":"q","kind":"Open",
            "options":[],"multiple":false}]},"answers":[],"index":0,
            "created_ms":0}"#;
        let state: State = serde_json::from_str(old).expect("old state should load");
        assert_eq!(state.message_id, None);
        assert_eq!(state.ask.questions.len(), 1);
    }

    #[test]
    fn recording_an_answer_changes_the_serialized_state() {
        // The compare-and-set in `advance_form` is against these bytes, so a
        // recorded answer has to change them or the guard would pass twice and
        // one question could be answered two ways.
        let state = State::new(
            "s",
            "c",
            "u",
            ask_of(r#"{"questions":[{"question":"first"},{"question":"second"}]}"#),
            0,
        );
        let before = serde_json::to_string(&state).unwrap();
        let mut after = state.clone();
        after.record(Answer {
            skipped: false,
            text: "yes".into(),
        });
        assert_ne!(before, serde_json::to_string(&after).unwrap());
    }
}
