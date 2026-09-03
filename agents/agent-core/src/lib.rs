//! The Thetis agent.
//!
//! This is the loop the grip exists to run — and the code the agent can
//! rewrite. It holds no state between turns: every turn rehydrates the
//! conversation from the session log, so a crash, a hot swap, or an
//! orchestrator restart costs nothing.
//!
//! The shape of one turn:
//!   rehydrate -> assemble context -> stream a completion -> dispatch tool
//!   calls -> check the inbox for nudges -> repeat until the model stops.

wit_bindgen::generate!({
    world: "agent",
    path: "../../wit",
    generate_all,
});

use thetis::grip::llm;
use thetis::grip::session as host;
use thetis::grip::skills;
use thetis::grip::sys;
use thetis::grip::types::{
    AssistantMsg, InboxItem, LlmError, LogLevel, SessionEvent, StreamChunk, TokenUsage, ToolCall,
    ToolOutcome, UserMsg,
};
// `tool-manifest` comes in via the world's own `use types.{...}`.
use serde_json::{json, Value};

mod compaction;
mod tools;

struct Component;



impl Guest for Component {
    fn health() -> String {
        "ok".to_string()
    }

    fn describe() -> AgentManifest {
        AgentManifest {
            name: "agent-core".to_string(),
            version_note: "streaming loop with nudges and memory tools".to_string(),
            skills: tools::available(tools::DEFAULT_MODE)
                .iter()
                .map(|t| t.name.to_string())
                .collect(),
        }
    }

    fn list_tools(mode: String) -> Vec<ToolManifest> {
        tools::manifests(&mode)
    }

    fn handle_turn(session_id: String) -> Result<TurnStats, String> {
        Turn::new(session_id).run()
    }
}

// --- configuration ----------------------------------------------------------

fn config_str(key: &str, fallback: &str) -> String {
    sys::config_get(key).unwrap_or_else(|| fallback.to_string())
}

// --- the turn ---------------------------------------------------------------

struct Turn {
    session_id: String,
    model: String,
    /// How the user asked this session to behave. Decides which tools the
    /// model is offered.
    mode: String,
    max_iterations: u32,
    /// The conversation as the model sees it.
    messages: Vec<Value>,
    /// The log sequence each message came from, in step with `messages`, so a
    /// compaction can be recorded against the log rather than against this
    /// particular rebuilding of it. The system prompt has no source, and takes 0.
    origins: Vec<u64>,
    /// The provider's own count of everything it was sent last turn: system
    /// prompt, tool schemas and the whole history. This is what compaction
    /// triggers on - an estimate of one part of the request would not do.
    context_tokens: u32,
    iterations: u32,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost_usd: f64,
    tools_used: Vec<String>,
    stopped_by: &'static str,
}

impl Turn {
    fn new(session_id: String) -> Self {
        // The session's own choices win over the grip defaults.
        let meta = host::get_session(&session_id);
        let model = meta
            .as_ref()
            .map(|m| m.model.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| config_str("model", "anthropic/claude-sonnet-4.5"));
        let mode = meta
            .as_ref()
            .map(|m| m.mode.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| tools::DEFAULT_MODE.to_string());

        Self {
            session_id,
            model,
            mode,
            max_iterations: u32::MAX,
            messages: Vec::new(),
            origins: Vec::new(),
            context_tokens: 0,
            iterations: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cost_usd: 0.0,
            tools_used: Vec::new(),
            stopped_by: "stop",
        }
    }

    fn run(mut self) -> Result<TurnStats, String> {
        self.rehydrate();
        // Before the first completion, so the cards are present for the answer
        // they were chosen for rather than the one after it.
        self.retrieve_skills_once();
        self.maybe_compact();

        loop {
            if self.iterations >= self.max_iterations {
                self.stopped_by = "max-iterations";
                self.note(&format!(
                    "stopped after {} iterations",
                    self.max_iterations
                ));
                break;
            }
            self.iterations += 1;

            let reply = match self.stream_completion() {
                Ok(reply) => reply,
                // Returning the error is enough: the orchestrator records the
                // incident, so logging it here too would double-report it.
                Err(e) => {
                    self.stopped_by = "llm-error";
                    return Err(e);
                }
            };

            // Persist what the model said before acting on it, so the log is
            // truthful even if a tool call traps.
            let seq = host::append(
                &self.session_id,
                &SessionEvent::AssistantMessage(AssistantMsg {
                    content: reply.text.clone(),
                    tool_calls: reply.tool_calls.clone(),
                    model: reply.model.clone(),
                    usage: reply.usage.clone(),
                }),
            );
            self.record_usage(&reply.usage);
            self.push(assistant_message(&reply), seq);

            if reply.tool_calls.is_empty() {
                // The model is done talking. Only a nudge that landed while it
                // was finishing justifies another round trip.
                match self.drain_inbox() {
                    Interrupt::None => {
                        self.stopped_by = "stop";
                        break;
                    }
                    Interrupt::Cancelled => {
                        self.stopped_by = "cancelled";
                        break;
                    }
                    Interrupt::Nudged => continue,
                }
            }

            for call in &reply.tool_calls {
                self.dispatch(call);
            }

            if matches!(self.drain_inbox(), Interrupt::Cancelled) {
                self.stopped_by = "cancelled";
                break;
            }
        }

        Ok(TurnStats {
            iterations: self.iterations,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cost_usd: self.cost_usd,
            tools_used: self.tools_used,
            stopped_by: self.stopped_by.to_string(),
        })
    }

    /// Rebuilds the model's view of the conversation from the event log.
    ///
    /// Compactions are applied as a projection: the events they cover are
    /// skipped and a summary note takes their place. Nothing is deleted, so an
    /// earlier compaction never costs us the ability to read the original.
    fn rehydrate(&mut self) {
        self.messages.clear();
        self.origins.clear();

        self.push(
            json!({ "role": "system", "content": self.system_prompt() }),
            0,
        );

        let records = host::events(&self.session_id, 0);

        // Which sequences a summary now stands for, and where each note goes.
        let mut covered: Vec<(u64, u64)> = Vec::new();
        let mut notes: Vec<(u64, Value)> = Vec::new();
        for record in &records {
            if let SessionEvent::ContextCompacted(c) = &record.event {
                let (Some(first), Some(last)) = (c.spans.first(), c.spans.last()) else {
                    continue;
                };
                for span in &c.spans {
                    covered.push((span.from_seq, span.through_seq));
                }
                notes.push((
                    first.from_seq,
                    compaction::note(
                        &c.summary,
                        c.messages_replaced,
                        first.from_seq,
                        last.through_seq,
                    ),
                ));
            }
        }

        for record in records {
            if let Some(i) = notes.iter().position(|(seq, _)| *seq == record.seq) {
                let (_, note) = notes.remove(i);
                self.push(note, record.seq);
            }
            if covered
                .iter()
                .any(|(from, through)| record.seq >= *from && record.seq <= *through)
            {
                continue;
            }
            let seq = record.seq;
            match record.event {
                SessionEvent::UserMessage(msg) => {
                    self.push(json!({ "role": "user", "content": user_content(&msg) }), seq);
                }
                SessionEvent::Nudge(text) => {
                    self.push(json!({ "role": "user", "content": text }), seq);
                }
                SessionEvent::AssistantMessage(msg) => {
                    // The last one wins: this ends up holding what the provider
                    // charged for the most recent request.
                    if let Some(usage) = &msg.usage {
                        self.context_tokens = usage.prompt_tokens;
                    }
                    let reply = Reply {
                        text: msg.content,
                        tool_calls: msg.tool_calls,
                        model: msg.model,
                        usage: msg.usage,
                    };
                    self.push(assistant_message(&reply), seq);
                }
                SessionEvent::ToolResult(out) => {
                    self.push(
                        json!({
                            "role": "tool",
                            "tool_call_id": out.call_id,
                            "content": out.content,
                        }),
                        seq,
                    );
                }
                SessionEvent::SystemNote(text) => {
                    // Deliberately not a `system` message. A note is appended
                    // wherever the log happens to be - after a tool result, or
                    // between two user turns - and a `system` message that
                    // neither precedes an assistant turn nor ends the array is
                    // rejected outright by Anthropic. The marker keeps it from
                    // reading as something the user said.
                    self.push(
                        json!({ "role": "user", "content": format!("[system note] {text}") }),
                        seq,
                    );
                }
                // Bookkeeping events carry no conversational meaning.
                _ => {}
            }
        }
    }

    /// Adds a message and the log sequence it came from together, so the two
    /// lists cannot drift apart.
    fn push(&mut self, message: Value, seq: u64) {
        self.messages.push(message);
        self.origins.push(seq);
    }

    /// Summarizes the oldest low-value stretch of the conversation when the
    /// context has grown past its threshold.
    ///
    /// Runs before the turn rather than after it, so the turn about to happen is
    /// the one that benefits. A failure is not fatal: the conversation simply
    /// stays long, which is worse than compacting and better than not answering.
    fn maybe_compact(&mut self) {
        let policy = compaction::Policy::load();
        if !policy.should_compact(self.context_tokens) {
            return;
        }

        let Some(plan) =
            compaction::plan(&self.messages, &self.origins, self.context_tokens, &policy)
        else {
            return;
        };

        let replaced = plan.messages_replaced;
        host::append(&self.session_id, &SessionEvent::ContextCompacted(plan));

        // Rebuild through the compaction just recorded.
        self.rehydrate();
        sys::log(
            LogLevel::Info,
            &format!("compaction: {replaced} messages replaced by a summary"),
        );
    }

    /// The base prompt, the mode's own instructions, and the skills this
    /// conversation can see.
    ///
    /// Skills arrive at two levels of detail. Universal briefs are always
    /// named, so the model knows they exist; the cards retrieved for this
    /// conversation get their `when_to_use` as well, because those are the ones
    /// likely to matter here. Neither carries a body — that is what
    /// `skill_fetch` is for, and the whole point is that a large corpus costs a
    /// constant amount of context.
    ///
    /// This must be byte-identical from one turn to the next or the provider's
    /// prompt cache misses on every call. That is why retrieval is pinned once
    /// and read back here rather than re-ranked per turn.
    fn system_prompt(&self) -> String {
        let mut prompt = config_str("system_prompt", "You are a helpful assistant.");

        // What the active mode is for. Withholding tools tells the model what it
        // cannot do, but never what it should do instead - left to itself it
        // just meets the gap and works around it.
        if let Some(mode) = sys::list_modes().into_iter().find(|m| m.id == self.mode) {
            if !mode.prompt.trim().is_empty() {
                prompt.push_str(&format!("\n\n# Mode: {}\n{}", mode.label, mode.prompt));
            }
        }

        let universal = skills::universal();
        let pinned = skills::pinned(&self.session_id);

        if !universal.is_empty() {
            prompt.push_str("\n\n# Always-available skills\n");
            prompt.push_str("Fetch one with `skill_fetch` when it applies.\n");
            for card in &universal {
                prompt.push_str(&format!("\n- `{}` — {}", card.id, card.brief));
            }
            prompt.push('\n');
        }

        // Only the ones retrieval added on top of the universals; repeating a
        // universal here would spend context saying the same thing twice.
        let extra: Vec<_> = pinned
            .iter()
            .filter(|c| !universal.iter().any(|u| u.id == c.id))
            .collect();

        if !extra.is_empty() {
            prompt.push_str("\n# Skills retrieved for this conversation\n");
            prompt.push_str(
                "Chosen from the corpus for the opening message. \
                 Fetch the body with `skill_fetch` before relying on one.\n",
            );
            for card in extra {
                prompt.push_str(&format!("\n- `{}` — {}", card.id, card.brief));
                if !card.when_to_use.trim().is_empty() {
                    prompt.push_str(&format!("\n  Use when: {}", card.when_to_use.trim()));
                }
                if !card.children.is_empty() {
                    prompt.push_str(&format!("\n  Nested: {}", card.children.join(", ")));
                }
            }
            prompt.push('\n');
        }

        prompt
    }

    /// Ranks the corpus against the opening message, once per conversation.
    ///
    /// Runs before the first completion of the first turn, because a skill that
    /// arrives after the model has already answered has missed its purpose. The
    /// result is pinned host-side, so every later turn reads back the same set
    /// and the system prompt stays stable.
    ///
    /// Best-effort throughout: retrieval failing means the model works without
    /// the extra cards, which is the situation before any of this existed.
    fn retrieve_skills_once(&mut self) {
        if !skills::pinned(&self.session_id).is_empty() {
            return;
        }

        // The opening user message is the query. Later messages are steering
        // within a task the first one already described.
        let Some(query) = self.first_user_message() else {
            return;
        };
        if query.trim().is_empty() {
            return;
        }

        let chosen = skills::retrieve(&self.session_id, &query, 0);
        if chosen.is_empty() {
            return;
        }

        sys::log(
            LogLevel::Debug,
            &format!(
                "skills retrieved: {}",
                chosen
                    .iter()
                    .map(|c| format!("{} {:.2}", c.id, c.score))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );

        // The prompt was built before the pin existed, so rebuild it now.
        self.rehydrate();
    }

    /// The text of the first thing the user said in this conversation.
    fn first_user_message(&self) -> Option<String> {
        host::events(&self.session_id, 0).into_iter().find_map(|r| {
            if let SessionEvent::UserMessage(msg) = r.event {
                Some(msg.text)
            } else {
                None
            }
        })
    }

    fn stream_completion(&mut self) -> Result<Reply, String> {
        let request = json!({
            "model": self.model,
            "messages": self.messages,
            "tools": tools::definitions(&self.mode),
        });

        let stream = llm::stream_open(&request.to_string()).map_err(describe_llm_error)?;

        let mut reply = Reply::default();
        loop {
            match llm::stream_next(stream) {
                Ok(StreamChunk::Delta(chunk)) => {
                    // Straight to the browser: the user sees tokens as they land.
                    host::emit_output(&self.session_id, &chunk);
                    reply.text.push_str(&chunk);
                }
                Ok(StreamChunk::ToolCalls(calls)) => reply.tool_calls = calls,
                Ok(StreamChunk::Finished(info)) => {
                    reply.model = info.model;
                    reply.usage = info.usage;
                    break;
                }
                Err(e) => {
                    llm::stream_close(stream);
                    return Err(describe_llm_error(e));
                }
            }
        }
        llm::stream_close(stream);
        Ok(reply)
    }

    fn dispatch(&mut self, call: &ToolCall) {
        host::append(&self.session_id, &SessionEvent::ToolInvocation(call.clone()));
        if !self.tools_used.iter().any(|n| n == &call.name) {
            self.tools_used.push(call.name.clone());
        }

        let outcome = tools::invoke(&self.session_id, &self.mode, &call.name, &call.arguments_json);
        let result = ToolOutcome {
            call_id: call.id.clone(),
            name: call.name.clone(),
            ok: outcome.is_ok(),
            content: match &outcome {
                Ok(content) => content.clone(),
                Err(message) => message.clone(),
            },
        };

        let seq = host::append(&self.session_id, &SessionEvent::ToolResult(result.clone()));
        self.push(
            json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": result.content,
            }),
            seq,
        );
    }

    /// Folds any mid-turn input into the conversation and reports what it found.
    fn drain_inbox(&mut self) -> Interrupt {
        let mut interrupt = Interrupt::None;

        for item in host::poll_inbox(&self.session_id) {
            match item {
                InboxItem::Nudge(text) => {
                    sys::log(LogLevel::Info, &format!("nudged mid-turn: {text}"));
                    // The host logs the nudge itself; this is the same text
                    // reaching the model within the turn already running, so it
                    // has no sequence of its own to point at.
                    self.push(json!({ "role": "user", "content": text }), 0);
                    // Cancellation outranks a nudge; never downgrade it.
                    if !matches!(interrupt, Interrupt::Cancelled) {
                        interrupt = Interrupt::Nudged;
                    }
                }
                InboxItem::Cancel => interrupt = Interrupt::Cancelled,
                InboxItem::Control(cmd) => {
                    sys::log(LogLevel::Debug, &format!("ignoring control item: {cmd}"));
                }
            }
        }

        interrupt
    }

    fn record_usage(&mut self, usage: &Option<TokenUsage>) {
        if let Some(u) = usage {
            self.prompt_tokens += u.prompt_tokens;
            self.completion_tokens += u.completion_tokens;
            self.cost_usd += u.cost_usd;
        }
    }

    fn note(&self, text: &str) {
        host::append(&self.session_id, &SessionEvent::SystemNote(text.to_string()));
    }
}

/// What, if anything, arrived from the user while the turn was running.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Interrupt {
    None,
    Nudged,
    Cancelled,
}

#[derive(Default)]
struct Reply {
    text: String,
    tool_calls: Vec<ToolCall>,
    model: String,
    usage: Option<TokenUsage>,
}

/// Builds the `content` field for a user message.
///
/// Plain text stays a bare string, which every provider accepts; attachments
/// promote it to the multi-part form with inline data URLs.
fn user_content(msg: &UserMsg) -> Value {
    if msg.attachments.is_empty() {
        return json!(msg.text);
    }

    let mut parts = Vec::new();
    if !msg.text.trim().is_empty() {
        parts.push(json!({ "type": "text", "text": msg.text }));
    }
    for attachment in &msg.attachments {
        if attachment.mime.starts_with("image/") {
            parts.push(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", attachment.mime, attachment.data_base64)
                }
            }));
        } else {
            // Nothing sensible to send inline, but the model should still know
            // the file was there.
            parts.push(json!({
                "type": "text",
                "text": format!("[attached file: {} ({})]", attachment.name, attachment.mime)
            }));
        }
    }
    json!(parts)
}

fn assistant_message(reply: &Reply) -> Value {
    let mut msg = json!({ "role": "assistant", "content": reply.text });
    if !reply.tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(
            reply
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments_json },
                    })
                })
                .collect(),
        );
    }
    msg
}

fn describe_llm_error(e: LlmError) -> String {
    match e {
        LlmError::Auth(d) => format!("authentication failed: {d}"),
        LlmError::RateLimited(d) => format!("rate limited: {d}"),
        LlmError::Transport(d) => format!("transport error: {d}"),
        LlmError::ModelError(d) => format!("model error: {d}"),
        LlmError::Budget(d) => format!("spend limit reached: {d}"),
        LlmError::BadRequest(d) => format!("bad request: {d}"),
    }
}

export!(Component);
