//! Summarizing a long conversation down.
//!
//! The selection rules are ported from a MOO agent grip where they were
//! worked out in anger, and they are the whole point of this module — deciding
//! *what* to shed matters far more than the summary prompt does:
//!
//!   * The head and the tail are kept verbatim. The head carries the framing
//!     that everything after it refers back to; the tail is the agent's live
//!     working memory, and collapsing it makes the next turn incoherent.
//!   * User messages are never summarized, anywhere. They are the human's
//!     steering, they are short, and paraphrasing them loses the instruction.
//!   * The middle is broken into *rounds* — an assistant tool-call turn plus the
//!     tool results that answer it, or a standalone assistant message. Rounds
//!     are summarized oldest-first and the walk stops at the first round
//!     boundary that gets under the target, so a long conversation sheds what it
//!     must and keeps the rest.
//!   * A round is never split, and a span never crosses a preserved user
//!     message, so the rebuilt history is still a valid request: every
//!     `tool` message still follows the assistant turn that called it.
//!
//! Thetis differs from the original in one way worth stating. There, originals
//! were copied into a side "offload store" before being dropped. Here the event
//! log is append-only and compaction never edits it — a compaction records
//! *which spans it stands for*, and rehydration projects the log through those
//! records. The originals are simply still there, so the store is unnecessary.

use crate::thetis::grip::llm;
use crate::thetis::grip::session as host;
use crate::thetis::grip::sys;
use crate::thetis::grip::types::{
    Compaction, CompactionProgress, InboxItem, LogLevel, SeqSpan,
};
use serde_json::{json, Value};

/// Tokens per character, near enough. Only used to rank and total messages
/// against each other; the trigger itself uses the provider's own count.
fn est_tokens(value: &Value) -> u32 {
    ((char_count(value) + 3) / 4) as u32
}

/// The estimated size of a stretch of the message list.
///
/// The trigger prefers the provider's own count, but there are two moments in a
/// turn where no such count exists: messages appended since the last completion
/// — tool results, which are where a turn's context growth actually comes from —
/// and the list immediately after a compaction, when the newest figure in the
/// log still describes the conversation as it was *before* the summary. This is
/// what fills both gaps.
pub fn estimate(messages: &[Value]) -> u32 {
    messages
        .iter()
        .fold(0u32, |acc, m| acc.saturating_add(est_tokens(m)))
}

/// Every character reachable in a message, including nested tool calls.
fn char_count(value: &Value) -> usize {
    match value {
        Value::String(s) => s.chars().count(),
        Value::Array(items) => items.iter().map(char_count).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.chars().count() + char_count(v))
            .sum(),
        Value::Number(n) => n.to_string().len(),
        Value::Bool(_) => 5,
        Value::Null => 4,
    }
}

fn role_of(msg: &Value) -> &str {
    msg.get("role").and_then(Value::as_str).unwrap_or("")
}

fn has_tool_calls(msg: &Value) -> bool {
    msg.get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

/// Settings, read from the host rather than assumed.
pub struct Policy {
    pub enabled: bool,
    pub window: u32,
    pub threshold: f64,
    pub target: f64,
    pub summary_model: String,
    pub keep_head: usize,
    pub keep_tail: usize,
}

impl Policy {
    pub fn load() -> Self {
        let num = |key: &str, fallback: f64| -> f64 {
            sys::config_get(key)
                .and_then(|v| v.parse().ok())
                .unwrap_or(fallback)
        };
        Self {
            enabled: sys::config_get("compact_enabled").as_deref() != Some("false"),
            window: num("context_window", 200_000.0) as u32,
            threshold: num("compact_threshold", 0.6),
            target: num("compact_target", 0.25),
            summary_model: sys::config_get("summary_model").unwrap_or_default(),
            keep_head: num("keep_head", 4.0) as usize,
            keep_tail: num("keep_tail", 30.0) as usize,
        }
    }

    fn trigger_tokens(&self) -> u32 {
        (f64::from(self.window) * self.threshold) as u32
    }

    fn target_tokens(&self) -> u32 {
        (f64::from(self.window) * self.target) as u32
    }

    /// Whether a context this size is worth compacting.
    pub fn should_compact(&self, context_tokens: u32) -> bool {
        self.enabled && context_tokens > 0 && context_tokens > self.trigger_tokens()
    }
}

/// A self-contained unit of the middle: `[start, end]` inclusive, message
/// indices. Never contains a user message, never splits a tool round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Round {
    start: usize,
    end: usize,
}

/// The stretch of the message list eligible for summarizing, after the head and
/// tail are set aside and the edges nudged off orphaned tool results.
///
/// Returns `None` when nothing is eligible.
fn eligible_middle(messages: &[Value], keep_head: usize, keep_tail: usize) -> Option<(usize, usize)> {
    let n = messages.len();
    // Index 0 is the system prompt, which is never part of the conversation.
    let mut lo = 1 + keep_head;
    let mut hi = n.checked_sub(keep_tail + 1)?;

    // A `tool` message whose assistant turn sits in the preserved head would be
    // orphaned by summarizing it, so walk past those.
    while lo <= hi && role_of(&messages[lo]) == "tool" {
        lo += 1;
    }
    // Likewise, do not leave the tail beginning on a bare `tool`: pull the
    // boundary back so that round stays whole inside the tail.
    while hi >= lo && messages.get(hi + 1).is_some_and(|m| role_of(m) == "tool") {
        hi = hi.checked_sub(1)?;
    }

    (hi >= lo).then_some((lo, hi))
}

/// Breaks `[lo, hi]` into the smallest self-contained summarizable units.
fn rounds(messages: &[Value], lo: usize, hi: usize) -> Vec<Round> {
    let mut out = Vec::new();
    let mut i = lo;
    while i <= hi {
        // User messages are not summarizable, and so become the natural break
        // points between rounds.
        if role_of(&messages[i]) == "user" {
            i += 1;
            continue;
        }
        let start = i;
        if role_of(&messages[i]) == "assistant" && has_tool_calls(&messages[i]) {
            i += 1;
            while i <= hi && role_of(&messages[i]) == "tool" {
                i += 1;
            }
        } else {
            i += 1;
        }
        out.push(Round { start, end: i - 1 });
    }
    out
}

/// Picks rounds oldest-first until enough has been shed, stopping at the first
/// round boundary that meets `need`.
fn select(messages: &[Value], rounds: &[Round], need: u32) -> Vec<Round> {
    let mut selected = Vec::new();
    let mut shed: u32 = 0;
    for round in rounds {
        if shed >= need {
            break;
        }
        shed = shed.saturating_add(
            (round.start..=round.end)
                .map(|i| est_tokens(&messages[i]))
                .sum(),
        );
        selected.push(*round);
    }
    selected
}

/// Groups selected rounds into contiguous spans. A gap — a preserved user
/// message sitting between two selected rounds — breaks the span, so no summary
/// ever spans across something the human said.
fn spans(selected: &[Round]) -> Vec<Round> {
    let mut out: Vec<Round> = Vec::new();
    for round in selected {
        match out.last_mut() {
            Some(open) if round.start == open.end + 1 => open.end = round.end,
            _ => out.push(*round),
        }
    }
    out
}

/// Flattens a span into plain text for the summarizer.
fn transcript(messages: &[Value], span: Round) -> String {
    let mut out = String::new();
    for msg in &messages[span.start..=span.end] {
        let content = msg
            .get("content")
            .map(|c| match c {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        out.push_str(&format!("[{}] {content}\n", role_of(msg)));

        for call in msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            out.push_str(&format!("  -> call {name} {args}\n"));
        }
    }
    out
}

const SUMMARY_INSTRUCTIONS: &str = "\
You are compacting the message history of an autonomous coding agent so it can \
keep working with far less context. Summarize the span below. Be concise, but \
preserve everything load-bearing. Use these sections:

1. Session intent: the user's overall goal and any explicit constraints.
2. Progress so far: what has actually been done, with the concrete file paths, \
component names, and identifiers involved.
3. Key facts learned: findings from inspection that later steps depend on.
4. Open threads / next steps: what remains.

Do not invent anything. Do not editorialise. If something was inconclusive, say \
so rather than resolving it. Here is the span:

";

/// One span of the conversation, flattened and ready to summarize.
///
/// Selection is separated from summarizing deliberately. Choosing what to shed
/// is pure and instant; summarizing is several model calls that can take tens of
/// seconds altogether. Handing the caller a list of jobs lets it report
/// progress between them and check whether the user has pressed stop — neither
/// of which was possible while the whole thing was one opaque function.
pub struct SpanJob {
    /// The span as plain text, with the instructions not yet attached.
    transcript: String,
    /// The log sequences this span covers, inclusive.
    pub first_seq: u64,
    pub last_seq: u64,
    /// How many messages it stands for.
    pub messages: u32,
}

/// One summary, from a separate and usually cheaper model.
///
/// A failure here returns `None` and the span is left alone: an unsummarized
/// span costs context, while a wrong summary costs correctness.
pub fn summarize(job: &SpanJob, model: &str) -> Option<String> {
    let request = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": format!("{SUMMARY_INSTRUCTIONS}{}", job.transcript),
        }],
    });

    let raw = match llm::chat(&request.to_string()) {
        Ok(raw) => raw,
        Err(_) => {
            sys::log(LogLevel::Warn, "compaction: the summary call failed");
            return None;
        }
    };

    let text = serde_json::from_str::<Value>(&raw)
        .ok()?
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)?;

    (!text.trim().is_empty()).then_some(text)
}

/// The note a summary becomes in the rebuilt history.
///
/// It takes the `user` role deliberately: that is what keeps a later compaction
/// from summarizing a summary, since user messages are never eligible.
pub fn note(summary: &str, replaced: u32, first_seq: u64, last_seq: u64) -> Value {
    json!({
        "role": "user",
        "content": format!(
            "[context compacted: {replaced} earlier messages, originally events \
             {first_seq}-{last_seq}, summarized below. The originals are still in \
             this session's log if a detail is missing.]\n\n{summary}"
        ),
    })
}

/// The one place a progress frame is built, so every phase reports the same
/// fields and the surface can rely on them.
fn report(
    session_id: &str,
    policy: &Policy,
    context_tokens: u32,
    phase: &str,
    span: u32,
    spans: u32,
    messages: u32,
    detail: &str,
) {
    host::emit_compaction_progress(
        session_id,
        &CompactionProgress {
            phase: phase.to_string(),
            span,
            spans,
            messages,
            tokens_before: context_tokens,
            tokens_target: policy.target_tokens(),
            model: policy.summary_model.clone(),
            detail: detail.to_string(),
        },
    );
}

/// Whether the user has pressed stop while compaction is running.
///
/// Compaction is not part of the conversation, so anything it finds in the
/// inbox has to be put back for the turn loop to act on: a nudge consumed here
/// would be silently lost, and a cancel consumed here would leave the loop
/// thinking the turn is fine. The host re-synthesizes a cancel on every later
/// poll, so only a nudge needs actually carrying — which the caller does by
/// draining the inbox itself once compaction is out of the way.
fn stop_requested(session_id: &str, carried: &mut Vec<String>) -> bool {
    let mut cancelled = false;
    for item in host::poll_inbox(session_id) {
        match item {
            InboxItem::Cancel => cancelled = true,
            InboxItem::Nudge(text) => carried.push(text),
            InboxItem::Control(_) => {}
        }
    }
    cancelled
}

/// What compaction decided to do, before any summarizing has happened.
pub struct Plan {
    pub jobs: Vec<SpanJob>,
    pub tokens_before: u32,
}

impl Plan {
    /// Total messages every span stands for.
    pub fn messages(&self) -> u32 {
        self.jobs.iter().map(|j| j.messages).sum()
    }
}

/// Summarizes a plan, reporting progress and stopping if the user asks.
///
/// Returns the `Compaction` to record, plus any nudge text that arrived while
/// this was running and must be handed back to the turn loop. `None` means
/// nothing usable came out — every span failed, or the user stopped it — and in
/// that case no `ContextCompacted` event should be written at all: a partial
/// compaction is fine, but an empty one that claims to stand for messages is
/// not.
pub fn run(session_id: &str, plan: Plan, policy: &Policy) -> (Option<Compaction>, Vec<String>) {
    let total = plan.jobs.len() as u32;
    let tokens_before = plan.tokens_before;
    let mut carried: Vec<String> = Vec::new();

    let mut summaries = Vec::new();
    let mut covered = Vec::new();
    let mut replaced = 0u32;

    for (i, job) in plan.jobs.iter().enumerate() {
        // Checked before each call rather than only after the batch: a stop
        // pressed during span one must not be answered by four more summary
        // calls. The host also fails the call itself now, but a guest that
        // relied on that alone would still make every request.
        if stop_requested(session_id, &mut carried) {
            report(
                session_id,
                policy,
                tokens_before,
                "cancelled",
                i as u32,
                total,
                replaced,
                "you stopped this turn; keeping what was summarized so far",
            );
            break;
        }

        report(
            session_id,
            policy,
            tokens_before,
            "summarizing",
            i as u32 + 1,
            total,
            replaced,
            &format!(
                "summarizing span {} of {total} ({} messages, events {}-{})",
                i + 1,
                job.messages,
                job.first_seq,
                job.last_seq
            ),
        );

        let Some(summary) = summarize(job, &policy.summary_model) else {
            continue;
        };
        summaries.push(format!(
            "### events {}-{}\n\n{summary}",
            job.first_seq, job.last_seq
        ));
        covered.push(SeqSpan {
            from_seq: job.first_seq,
            through_seq: job.last_seq,
        });
        replaced += job.messages;
    }

    if covered.is_empty() {
        report(
            session_id,
            policy,
            tokens_before,
            "failed",
            0,
            total,
            0,
            "nothing was summarized; the conversation is unchanged",
        );
        return (None, carried);
    }

    report(
        session_id,
        policy,
        tokens_before,
        "finished",
        total,
        total,
        replaced,
        &format!("{replaced} messages now stand summarized"),
    );

    (
        Some(Compaction {
            spans: covered,
            summary: summaries.join("\n\n"),
            messages_replaced: replaced,
            tokens_before,
        }),
        carried,
    )
}

/// Works out what to shed, without summarizing any of it.
///
/// `origins` maps each message to the log sequence it came from, so the result
/// can be recorded against the log rather than against this particular
/// rebuilding of it. Returns `None` when there is nothing worth doing.
pub fn plan(
    session_id: &str,
    messages: &[Value],
    origins: &[u64],
    context_tokens: u32,
    policy: &Policy,
) -> Option<Plan> {
    // The two lists are indexed together below. Drifting apart would mean
    // recording a summary against the wrong part of the log, so refuse rather
    // than guess - and a panic here would trap the whole turn.
    if origins.len() != messages.len() {
        sys::log(
            LogLevel::Error,
            "compaction: message and origin lists disagree; skipping",
        );
        return None;
    }

    // Announced before the selection walk, not after it. Selection is fast, but
    // this frame is also what tells the surface that a compaction is starting at
    // all — and the surface needs to know that before the first summary call,
    // which is the part that takes the time.
    report(
        session_id,
        policy,
        context_tokens,
        "planning",
        0,
        0,
        0,
        "choosing what to summarize",
    );

    let (lo, hi) = eligible_middle(messages, policy.keep_head, policy.keep_tail)?;
    let rounds = rounds(messages, lo, hi);
    if rounds.is_empty() {
        return None;
    }

    let need = context_tokens.saturating_sub(policy.target_tokens());
    let selected = select(messages, &rounds, need);
    if selected.is_empty() {
        return None;
    }
    let spans = spans(&selected);

    sys::log(
        LogLevel::Info,
        &format!(
            "compaction: summarizing {} round(s) in {} span(s); ~{context_tokens} tokens, \
             target ~{}",
            selected.len(),
            spans.len(),
            policy.target_tokens()
        ),
    );

    // Sequence 0 means "no source in the log": the system prompt, and nudge
    // text folded into a turn already running. Recording a span that starts
    // there would cover the log from its very beginning, silently summarizing
    // away the whole conversation. It should be impossible: a seq-0 push is
    // always a `user` message and `rounds` never puts one in a span. The check
    // stays because the cost of being wrong is the entire history, and mid-turn
    // compaction now runs at points where those pushes have happened.
    if spans
        .iter()
        .any(|s| origins[s.start] == 0 || origins[s.end] == 0)
    {
        sys::log(
            LogLevel::Error,
            "compaction: a span has no log sequence; skipping rather than \
             recording one that would cover the whole log",
        );
        return None;
    }

    let jobs: Vec<SpanJob> = spans
        .into_iter()
        .map(|span| SpanJob {
            transcript: transcript(messages, span),
            first_seq: origins[span.start],
            last_seq: origins[span.end],
            messages: (span.end - span.start + 1) as u32,
        })
        .collect();

    Some(Plan {
        jobs,
        tokens_before: context_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": text })
    }
    fn assistant(text: &str) -> Value {
        json!({ "role": "assistant", "content": text })
    }
    fn calling(name: &str) -> Value {
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "id": "c1", "function": { "name": name, "arguments": "{}" } }],
        })
    }
    fn tool(text: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": "c1", "content": text })
    }

    /// System prompt, then `n` alternating assistant/tool rounds.
    fn conversation(n: usize) -> Vec<Value> {
        let mut msgs = vec![json!({ "role": "system", "content": "sys" }), user("go")];
        for i in 0..n {
            msgs.push(calling(&format!("tool{i}")));
            msgs.push(tool(&format!("result {i}")));
        }
        msgs
    }

    #[test]
    fn a_round_is_an_assistant_turn_plus_the_results_that_answer_it() {
        let msgs = conversation(3);
        let found = rounds(&msgs, 2, msgs.len() - 1);
        assert_eq!(found.len(), 3);
        for round in &found {
            // Each round is exactly the call and its one result.
            assert_eq!(round.end - round.start, 1);
            assert!(has_tool_calls(&msgs[round.start]));
            assert_eq!(role_of(&msgs[round.end]), "tool");
        }
    }

    #[test]
    fn user_messages_are_never_part_of_a_round() {
        let msgs = vec![
            json!({ "role": "system", "content": "sys" }),
            calling("a"),
            tool("a"),
            user("actually, do this instead"),
            calling("b"),
            tool("b"),
        ];
        let found = rounds(&msgs, 1, msgs.len() - 1);
        assert_eq!(found.len(), 2);
        for round in &found {
            for i in round.start..=round.end {
                assert_ne!(role_of(&msgs[i]), "user", "a user message got swept into a round");
            }
        }
    }

    #[test]
    fn a_user_message_breaks_a_span_in_two() {
        // Rounds either side of a preserved user message are not contiguous, so
        // they must not be summarized together.
        let selected = vec![
            Round { start: 1, end: 2 },
            Round { start: 4, end: 5 },
            Round { start: 6, end: 7 },
        ];
        let grouped = spans(&selected);
        assert_eq!(grouped, vec![Round { start: 1, end: 2 }, Round { start: 4, end: 7 }]);
    }

    #[test]
    fn the_head_and_tail_are_never_eligible() {
        let msgs = conversation(20);
        let (lo, hi) = eligible_middle(&msgs, 4, 30).unwrap_or((0, 0));
        // 42 messages, keeping 4 at the head and 30 at the tail leaves little;
        // whatever is left must sit strictly inside those bounds.
        if hi >= lo {
            assert!(lo >= 5, "ate into the head");
            assert!(hi < msgs.len() - 30, "ate into the tail");
        }
    }

    #[test]
    fn nothing_is_eligible_in_a_short_conversation() {
        assert!(eligible_middle(&conversation(2), 4, 30).is_none());
    }

    #[test]
    fn the_middle_never_starts_on_an_orphaned_tool_result() {
        // The head ends mid-round, so message 5 is a `tool` whose assistant turn
        // is preserved above it. Summarizing it alone would strand it.
        let msgs = vec![
            json!({ "role": "system", "content": "sys" }),
            user("go"),
            assistant("thinking"),
            assistant("more"),
            calling("a"),
            tool("a"),
            calling("b"),
            tool("b"),
            assistant("done"),
        ];
        let (lo, _) = eligible_middle(&msgs, 4, 1).unwrap();
        assert_ne!(role_of(&msgs[lo]), "tool");
    }

    #[test]
    fn selection_stops_at_a_round_boundary_once_it_has_shed_enough() {
        let msgs = conversation(10);
        let found = rounds(&msgs, 2, msgs.len() - 1);
        // Ask for barely anything: one round should satisfy it.
        let picked = select(&msgs, &found, 1);
        assert_eq!(picked.len(), 1, "shed more than was needed");
        assert_eq!(picked[0], found[0], "did not start with the oldest");
    }

    #[test]
    fn selection_takes_more_when_more_is_needed() {
        let msgs = conversation(10);
        let found = rounds(&msgs, 2, msgs.len() - 1);
        let picked = select(&msgs, &found, u32::MAX);
        assert_eq!(picked.len(), found.len());
    }

    #[test]
    fn the_trigger_is_a_fraction_of_the_window() {
        let policy = Policy {
            enabled: true,
            window: 1000,
            threshold: 0.6,
            target: 0.25,
            summary_model: String::new(),
            keep_head: 4,
            keep_tail: 30,
        };
        assert!(!policy.should_compact(599));
        assert!(policy.should_compact(601));
        assert_eq!(policy.target_tokens(), 250);
        // An unknown context size is not a reason to compact.
        assert!(!policy.should_compact(0));
    }

    #[test]
    fn a_disabled_policy_never_triggers() {
        let policy = Policy {
            enabled: false,
            window: 1000,
            threshold: 0.6,
            target: 0.25,
            summary_model: String::new(),
            keep_head: 4,
            keep_tail: 30,
        };
        assert!(!policy.should_compact(999));
    }

    /// The progress card shows this number, so it has to be the sum of what the
    /// spans actually stand for and not the number of spans.
    #[test]
    fn a_plan_reports_the_messages_its_spans_stand_for() {
        let plan = Plan {
            tokens_before: 120_000,
            jobs: vec![
                SpanJob {
                    transcript: "a".into(),
                    first_seq: 3,
                    last_seq: 10,
                    messages: 8,
                },
                SpanJob {
                    transcript: "b".into(),
                    first_seq: 14,
                    last_seq: 19,
                    messages: 6,
                },
            ],
        };
        assert_eq!(plan.messages(), 14);
        assert_eq!(plan.jobs.len(), 2, "two spans, fourteen messages");
    }

    /// The mid-turn trigger rests on this: growth that no completion has been
    /// charged for yet still has to show up as growth.
    #[test]
    fn estimating_a_list_grows_as_the_list_grows() {
        let small = vec![assistant("ok")];
        let big = vec![assistant("ok"), tool(&"x".repeat(4000))];
        let (a, b) = (estimate(&small), estimate(&big));
        assert!(b > a, "adding a large tool result must raise the estimate");
        // Four characters to the token, so a 4000-char result is ~1000.
        assert!(
            (900..=1100).contains(&(b - a)),
            "a 4000-character result should estimate near 1000 tokens, got {}",
            b - a
        );
    }

    #[test]
    fn estimating_an_empty_list_is_zero() {
        // `context_estimate` slices from the billed boundary, which is the whole
        // list when a completion has just landed. That slice must contribute
        // nothing rather than panicking or inventing a figure.
        assert_eq!(estimate(&[]), 0);
    }

    /// A turn whose context has grown past the trigger since the last completion
    /// must compact, even though the provider's own figure has not moved. This is
    /// the bug: the count alone was checked once per turn and never budged, so a
    /// runaway agentic turn sailed past the window.
    #[test]
    fn unbilled_growth_can_cross_the_trigger_on_its_own() {
        let policy = Policy {
            enabled: true,
            window: 1000,
            threshold: 0.6,
            target: 0.25,
            summary_model: String::new(),
            keep_head: 4,
            keep_tail: 30,
        };
        // What the last request was charged: comfortably under the 600 trigger.
        let billed = 500u32;
        assert!(!policy.should_compact(billed), "not yet worth compacting");

        // Two big tool results land mid-turn. Nothing re-prices the request.
        let unbilled = vec![tool(&"x".repeat(2000)), tool(&"y".repeat(2000))];
        let combined = billed + estimate(&unbilled);
        assert!(
            policy.should_compact(combined),
            "≈{combined} tokens is over the trigger and must compact mid-turn"
        );
    }

    #[test]
    fn a_summary_note_is_a_user_message_so_it_is_never_resummarized() {
        let n = note("a summary", 12, 3, 40);
        assert_eq!(role_of(&n), "user");
        let content = n.get("content").and_then(Value::as_str).unwrap();
        assert!(content.contains("12 earlier messages"));
        assert!(content.contains("3-40"), "the note should say what it replaced");
        assert!(content.contains("a summary"));
    }
}
