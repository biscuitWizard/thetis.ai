//! Reading and searching conversation logs across the whole database.
//!
//! Every other path into the event log is scoped to one conversation:
//! `session.events` goes through `HostState::scope_ok`, the `store.events` IPC
//! arm goes through `own_session`, and `delegation.child-transcript` admits only
//! the caller's own children. Those scopes exist to stop a worker *writing* into
//! a conversation that is not its own, and to stop one forging events or
//! draining another's budget.
//!
//! This module is deliberately outside those scopes, and only this module is.
//! What it offers is recall: an agent asking "have I done this before", "what
//! did that sub-agent actually find", "which conversation was it where the build
//! broke this way". Answering that requires reading logs the asking session did
//! not write, so the boundary being crossed is real and worth stating plainly:
//!
//! * **Read only.** Nothing here opens a write transaction. There is no
//!   `append`, no rename, no settle. A caller gains the ability to *see* another
//!   conversation and nothing else, which is why widening the read did not
//!   require widening `own_session` for the arms that mutate.
//! * **Bounded.** Every function caps what it returns and says so when the
//!   answer is partial. A transcript scan over a long-lived database would
//!   otherwise be the cheapest way to exhaust a context window or a redb read
//!   transaction.
//! * **Clipped host-side.** Entries carry text already cut to a ceiling, so a
//!   conversation full of 32 KiB tool results cannot turn one call into a
//!   multi-megabyte IPC response. The guest never gets the chance to ask for
//!   more than the host is willing to move.
//!
//! Transient event arms — stream and reasoning deltas, compaction progress —
//! are absent from every projection here. They are never persisted, so they
//! could not be searched anyway; leaving them out of `entries_of` means one
//! place decides what an event *says*, and read and search cannot disagree
//! about it.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::bindings::types::{EventRecord, SessionEvent, SessionMeta};
use crate::store::Store;
use crate::subagents::SubagentRow;

/// Hits returned when the caller does not ask for a number.
pub const DEFAULT_MAX_RESULTS: usize = 100;
/// Ceiling on hits, whatever was asked for.
pub const MAX_RESULTS_CAP: usize = 500;
/// Entries returned by a read when the caller does not ask for a number.
pub const DEFAULT_READ_LIMIT: usize = 200;
/// Ceiling on entries in one read.
pub const READ_LIMIT_CAP: usize = 1000;
/// Characters per entry when the caller does not ask.
pub const DEFAULT_MAX_CHARS: usize = 600;
/// Ceiling on characters per entry. Generous enough for a stack trace, mean
/// enough that a page of them still fits a context window.
pub const MAX_CHARS_CAP: usize = 8000;
/// Conversations one search will open. A database with more than this is
/// searched newest-first and reports itself incomplete.
pub const SCAN_CONVERSATION_CAP: usize = 400;
/// Events one search will project across every conversation together.
pub const SCAN_EVENT_CAP: usize = 400_000;

/// One conversation, top-level or sub-agent, as a catalogue entry.
///
/// The sub-agent fields are empty for a top-level conversation rather than
/// living in an `Option<...>`: this record crosses the WIT boundary, where a
/// flat record with empty strings is cheaper to describe and read than a nested
/// optional, and "no label" and "empty label" mean the same thing here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub mode: String,
    pub model: String,
    pub preview: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub event_count: u64,
    pub archived: bool,
    /// True when this is a sub-agent's session rather than a conversation
    /// somebody started.
    pub is_subagent: bool,
    /// The session that spawned it; empty for a top-level conversation.
    pub parent_id: String,
    /// The top-level conversation it belongs to; its own id when it is one.
    pub root_id: String,
    /// The sub-agent's short label; empty for a top-level conversation.
    pub label: String,
    /// running | done | failed | cancelled, for a sub-agent; empty otherwise.
    pub state: String,
    /// The brief a sub-agent was given; empty for a top-level conversation.
    pub task: String,
}

impl ConversationSummary {
    fn from_meta(meta: SessionMeta, row: Option<&SubagentRow>) -> Self {
        Self {
            id: meta.id,
            title: meta.title,
            mode: meta.mode,
            model: meta.model,
            preview: meta.preview,
            created_ms: meta.created_ms,
            updated_ms: meta.updated_ms,
            event_count: meta.event_count,
            archived: meta.archived,
            is_subagent: row.is_some(),
            parent_id: row.map(|r| r.parent_id.clone()).unwrap_or_default(),
            root_id: row.map(|r| r.root_id.clone()).unwrap_or_default(),
            label: row.map(|r| r.label.clone()).unwrap_or_default(),
            state: row
                .map(|r| r.state.as_str().to_string())
                .unwrap_or_default(),
            task: row.map(|r| r.task.clone()).unwrap_or_default(),
        }
    }
}

/// One line of a transcript, projected from an event and already clipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub seq: u64,
    pub ts_ms: u64,
    /// What kind of thing this is: see [`Kind`].
    pub kind: String,
    pub text: String,
    /// Characters cut from `text`; 0 when it is whole. The caller can then say
    /// "there was more" rather than quoting a sentence that stops mid-word as
    /// though it were the whole record.
    pub elided: u64,
}

/// One search match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptHit {
    pub session_id: String,
    pub title: String,
    pub is_subagent: bool,
    /// The sub-agent's label, so a hit in a child is attributable without a
    /// second lookup.
    pub label: String,
    pub seq: u64,
    pub ts_ms: u64,
    pub kind: String,
    /// The matching line, clipped.
    pub text: String,
}

/// What a search found, and how sure it is that this is all of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReport {
    pub hits: Vec<TranscriptHit>,
    /// Conversations with at least one match.
    pub matched_conversations: u64,
    /// Matches found, including any past the hit cap.
    pub total_matches: u64,
    /// Conversations actually opened.
    pub scanned_conversations: u64,
    /// True when the hit cap stopped the scan.
    pub capped: bool,
    /// True when a scan bound was reached before every conversation was read.
    /// The scan is newest-first, so an incomplete answer is missing the oldest.
    pub incomplete: bool,
}

/// The kind tags an entry can carry. String-valued across the WIT boundary, but
/// named here so read and search cannot drift apart, and so a caller filtering
/// by kind has something to match against.
pub mod kind {
    pub const USER: &str = "user";
    pub const ASSISTANT: &str = "assistant";
    pub const TOOL_CALL: &str = "tool-call";
    pub const TOOL_RESULT: &str = "tool-result";
    pub const TOOL_FAILED: &str = "tool-failed";
    pub const NUDGE: &str = "nudge";
    pub const NOTE: &str = "note";
    pub const INCIDENT: &str = "incident";
    pub const MODIFICATION: &str = "modification";
    pub const BRANCH_OP: &str = "branch-op";
    pub const TURN_FINISHED: &str = "turn-finished";
    pub const COMPACTED: &str = "compacted";
}

/// What one search asks for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Rust regex syntax. `(?i)` for case-insensitive.
    pub pattern: String,
    /// One conversation, or empty for every one.
    pub session_id: String,
    pub include_archived: bool,
    pub include_subagents: bool,
    /// Whether successful tool output is searched.
    ///
    /// Off by default because tool output is the overwhelming majority of the
    /// bytes in a transcript and the great majority of the noise: file contents
    /// an agent read once, directory listings, whole other searches' results. A
    /// pattern that matches a file's text matches it in every conversation that
    /// ever read that file, which buries the conversation actually about it.
    ///
    /// **Failed** tool results are searched regardless. They are short, they are
    /// what someone grepping for an error message is looking for, and excluding
    /// them would make "find where this broke before" — the main reason to grep
    /// a transcript at all — silently impossible.
    pub include_tool_output: bool,
    pub max_results: usize,
    pub max_chars: usize,
}

/// Read-only access to every conversation's log.
///
/// Borrows the store rather than holding an `Arc`, matching
/// [`crate::subagents::Subagents`]: equally cheap from the gateway's
/// `Arc<Store>` and from the `&Store` the IPC store server is handed.
#[derive(Clone, Copy)]
pub struct Transcripts<'a> {
    store: &'a Store,
    owner: Option<&'a str>,
}

impl<'a> Transcripts<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store, owner: None }
    }

    /// Restricts every catalogue/read/search operation to one owner's roots.
    pub fn owned(store: &'a Store, owner: Option<&'a str>) -> Self {
        Self { store, owner }
    }

    fn mine(&self, session_id: &str) -> Result<bool> {
        Ok(match self.owner {
            None => true,
            Some(owner) => self.store.owner_of_root(session_id)?.as_deref() == Some(owner),
        })
    }

    /// Every conversation, most recently active first.
    ///
    /// `limit` of 0 means all of them. Sub-agents are excluded unless asked
    /// for, because a busy database has many more sub-agent sessions than
    /// conversations and listing them together buries the conversations.
    pub fn conversations(
        &self,
        include_archived: bool,
        include_subagents: bool,
        limit: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let mut out: Vec<ConversationSummary> = self
            .store
            .sessions_with_subagent_rows(include_archived)?
            .into_iter()
            .filter(|(meta, _)| self.mine(&meta.id).unwrap_or(false))
            .filter(|(_, row)| include_subagents || row.is_none())
            .map(|(meta, row)| ConversationSummary::from_meta(meta, row.as_ref()))
            .collect();
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        if limit > 0 {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// One conversation's own summary, sub-agent or not.
    pub fn conversation(&self, session_id: &str) -> Result<ConversationSummary> {
        if !self.mine(session_id)? {
            return Err(anyhow!("no conversation with id `{session_id}`"));
        }
        let meta = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no conversation with id `{session_id}`"))?;
        let row = self.store.get_subagent(session_id)?;
        Ok(ConversationSummary::from_meta(meta, row.as_ref()))
    }

    /// The sub-agents belonging to one conversation, whatever their depth.
    ///
    /// Unlike `delegation.children`, this answers for *any* conversation rather
    /// than only the caller's own, and it keys off `root_id`, so it reports a
    /// whole tree rather than one generation.
    pub fn subagents(&self, root_id: &str) -> Result<Vec<ConversationSummary>> {
        if !self.mine(root_id)? {
            return Err(anyhow!("no conversation with id `{root_id}`"));
        }
        let mut out = Vec::new();
        for row in self.store.subagents_under(root_id)? {
            // A registry row whose session is gone is skipped rather than
            // reported as an empty conversation: the row is bookkeeping, the
            // session is the thing being catalogued.
            if let Some(meta) = self.store.get_session(&row.child_id)? {
                out.push(ConversationSummary::from_meta(meta, Some(&row)));
            }
        }
        out.sort_by_key(|c| c.created_ms);
        Ok(out)
    }

    /// A window of one conversation's transcript, oldest first.
    ///
    /// `from_seq` is exclusive, matching `session.events`, so paging is
    /// `from_seq = last seq seen`. `limit` and `max_chars` of 0 take the
    /// defaults; both are clamped to their caps.
    pub fn read(
        &self,
        session_id: &str,
        from_seq: u64,
        limit: usize,
        max_chars: usize,
    ) -> Result<Vec<TranscriptEntry>> {
        // Confirm the session exists and belongs to this catalogue.
        if !self.mine(session_id)? || self.store.get_session(session_id)?.is_none() {
            return Err(anyhow!("no conversation with id `{session_id}`"));
        }
        let limit = clamp(limit, DEFAULT_READ_LIMIT, READ_LIMIT_CAP);
        let max_chars = clamp(max_chars, DEFAULT_MAX_CHARS, MAX_CHARS_CAP);

        let mut out = Vec::new();
        for record in self.store.events(session_id, from_seq)? {
            for (kind, text) in entries_of(&record.event, true) {
                let (text, elided) = clip(&text, max_chars);
                out.push(TranscriptEntry {
                    seq: record.seq,
                    ts_ms: record.ts_ms,
                    kind: kind.to_string(),
                    text,
                    elided,
                });
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Searches transcripts for a regular expression.
    ///
    /// Newest conversation first, and each conversation oldest event first: a
    /// capped answer then holds the most recent conversations in full rather
    /// than an arbitrary slice of every one.
    pub fn search(&self, query: &SearchQuery) -> Result<SearchReport> {
        if query.pattern.trim().is_empty() {
            return Err(anyhow!("a search needs a pattern"));
        }
        // Bounded so a pathological pattern cannot compile into hundreds of
        // megabytes of DFA, matching `hostfs::search_files`.
        let re = regex::RegexBuilder::new(&query.pattern)
            .size_limit(10 << 20)
            .build()
            .map_err(|e| anyhow!("bad pattern `{}`: {e}", query.pattern))?;

        let max_results = clamp(query.max_results, DEFAULT_MAX_RESULTS, MAX_RESULTS_CAP);
        let max_chars = clamp(query.max_chars, DEFAULT_MAX_CHARS, MAX_CHARS_CAP);

        let targets = if query.session_id.trim().is_empty() {
            self.conversations(query.include_archived, query.include_subagents, 0)?
        } else {
            vec![self.conversation(query.session_id.trim())?]
        };

        let mut report = SearchReport {
            hits: Vec::new(),
            matched_conversations: 0,
            total_matches: 0,
            scanned_conversations: 0,
            capped: false,
            incomplete: false,
        };
        let mut events_seen = 0usize;

        for conversation in targets {
            if report.scanned_conversations as usize >= SCAN_CONVERSATION_CAP
                || events_seen >= SCAN_EVENT_CAP
            {
                report.incomplete = true;
                break;
            }
            report.scanned_conversations += 1;
            let records = self.store.events(&conversation.id, 0)?;
            events_seen += records.len();

            let mut matched_here = false;
            for record in &records {
                for (kind, text) in entries_of(&record.event, query.include_tool_output) {
                    for line in matching_lines(&re, &text) {
                        report.total_matches += 1;
                        matched_here = true;
                        if report.hits.len() < max_results {
                            let (text, _) = clip(line, max_chars);
                            report.hits.push(TranscriptHit {
                                session_id: conversation.id.clone(),
                                title: conversation.title.clone(),
                                is_subagent: conversation.is_subagent,
                                label: conversation.label.clone(),
                                seq: record.seq,
                                ts_ms: record.ts_ms,
                                kind: kind.to_string(),
                                text,
                            });
                        } else {
                            report.capped = true;
                        }
                    }
                }
            }
            if matched_here {
                report.matched_conversations += 1;
            }
            // Once the cap is hit there is nothing to gain from opening more
            // conversations, but the count of what was *not* looked at would be
            // a lie if the scan simply stopped, so say it is incomplete.
            if report.capped {
                report.incomplete = true;
                break;
            }
        }

        Ok(report)
    }
}

/// Lines of `text` the pattern matches.
///
/// Line-wise rather than whole-text, because a hit is only useful if the caller
/// can be shown the part that matched: an assistant message can be pages long,
/// and reporting "it is in here somewhere" costs the same tokens as reporting
/// nothing. A single-line text yields itself.
fn matching_lines<'t>(re: &regex::Regex, text: &'t str) -> Vec<&'t str> {
    text.lines()
        .filter(|line| re.is_match(line))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

/// What an event contributes to a transcript, as (kind, text) pairs.
///
/// One event can contribute several: an assistant message with three tool calls
/// is one message line and three call lines, which is what makes a search for a
/// tool name work.
///
/// The transient arms contribute nothing. They are never persisted, so they
/// cannot appear in a stored log at all — handling them here is about having no
/// arm that a future contract addition can fall through silently, since the
/// match is exhaustive and a new event variant will fail to compile until
/// somebody decides what it says.
pub fn entries_of(event: &SessionEvent, include_tool_output: bool) -> Vec<(&'static str, String)> {
    match event {
        SessionEvent::UserMessage(msg) => {
            let mut text = msg.text.clone();
            if !msg.attachments.is_empty() {
                let names: Vec<&str> = msg.attachments.iter().map(|a| a.name.as_str()).collect();
                if !text.trim().is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("[attached: {}]", names.join(", ")));
            }
            if text.trim().is_empty() {
                return Vec::new();
            }
            vec![(kind::USER, text)]
        }
        SessionEvent::AssistantMessage(msg) => {
            let mut out = Vec::new();
            if !msg.content.trim().is_empty() {
                out.push((kind::ASSISTANT, msg.content.clone()));
            }
            for call in &msg.tool_calls {
                out.push((
                    kind::TOOL_CALL,
                    format!("{}({})", call.name, call.arguments_json),
                ));
            }
            out
        }
        // Skipped: the same call is already carried by the assistant message
        // that announced it, and reporting both would double every hit on a
        // tool name.
        SessionEvent::ToolInvocation(_) => Vec::new(),
        SessionEvent::ToolResult(out) => {
            // A failure is always searchable; see `SearchQuery::
            // include_tool_output` for why success is not.
            if out.ok && !include_tool_output {
                return Vec::new();
            }
            let tag = if out.ok {
                kind::TOOL_RESULT
            } else {
                kind::TOOL_FAILED
            };
            vec![(tag, format!("{}: {}", out.name, out.content))]
        }
        SessionEvent::Nudge(text) => vec![(kind::NUDGE, text.clone())],
        SessionEvent::SystemNote(text) => vec![(kind::NOTE, text.clone())],
        SessionEvent::Incident(text) => vec![(kind::INCIDENT, text.clone())],
        SessionEvent::Modification(m) => vec![(
            kind::MODIFICATION,
            format!(
                "{} {}: {}",
                m.aspect,
                if m.success { "ok" } else { "failed" },
                m.detail
            ),
        )],
        SessionEvent::BranchOp(op) => vec![(
            kind::BRANCH_OP,
            format!(
                "{} {}: {} ({} -> {})",
                op.op,
                if op.ok { "ok" } else { "failed" },
                op.detail,
                short_rev(&op.from_rev),
                short_rev(&op.to_rev)
            ),
        )],
        SessionEvent::TurnFinished(stats) => vec![(
            kind::TURN_FINISHED,
            format!(
                "stopped by {}, {} iterations, ${:.4}",
                stats.stopped_by, stats.iterations, stats.cost_usd
            ),
        )],
        SessionEvent::ContextCompacted(c) => vec![(
            kind::COMPACTED,
            format!(
                "[{} messages summarised] {}",
                c.messages_replaced, c.summary
            ),
        )],
        // Nothing to say, or never persisted.
        SessionEvent::TurnStarted
        | SessionEvent::StreamDelta(_)
        | SessionEvent::ReasoningDelta(_)
        | SessionEvent::CompactionProgress(_) => Vec::new(),
    }
}

fn short_rev(rev: &str) -> String {
    rev.chars().take(8).collect()
}

/// Clips to a character ceiling, reporting how much was dropped.
///
/// Counts characters and slices at a character boundary, so a multi-byte
/// character is never split — a truncated UTF-8 sequence would travel fine over
/// JSON as a replacement character and then be quoted back as garbage.
fn clip(text: &str, max_chars: usize) -> (String, u64) {
    let trimmed = text.trim();
    let total = trimmed.chars().count();
    if total <= max_chars {
        return (trimmed.to_string(), 0);
    }
    let kept: String = trimmed.chars().take(max_chars).collect();
    (kept, (total - max_chars) as u64)
}

/// `0` means "the default", and anything above the cap becomes the cap.
fn clamp(asked: usize, default: usize, cap: usize) -> usize {
    if asked == 0 { default } else { asked.min(cap) }
}

/// Whether a projection of `records` is what a reader would call empty.
/// Used by callers wanting to distinguish "no events" from "only transient
/// ones", which look identical in a rendered transcript.
pub fn is_silent(records: &[EventRecord]) -> bool {
    records
        .iter()
        .all(|r| entries_of(&r.event, true).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::types::{
        AssistantMsg, Attachment, ToolCall, ToolOutcome, TurnStats, UserMsg,
    };

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        (store, dir)
    }

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMsg {
            text: text.to_string(),
            attachments: vec![],
        })
    }

    fn assistant(text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage(AssistantMsg {
            content: text.to_string(),
            tool_calls: vec![],
            model: "test".into(),
            usage: None,
        })
    }

    fn tool_result(name: &str, ok: bool, content: &str) -> SessionEvent {
        SessionEvent::ToolResult(ToolOutcome {
            call_id: "c1".into(),
            name: name.to_string(),
            ok,
            content: content.to_string(),
        })
    }

    fn query(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_search_finds_a_message_in_another_conversation() {
        // The whole point of the feature: recall across conversations the
        // asking session did not write.
        let (store, _d) = temp_store();
        let a = store
            .create_session(Some("first".into()), &"agent", "local")
            .unwrap();
        let b = store
            .create_session(Some("second".into()), &"agent", "local")
            .unwrap();
        store
            .append_event(&a.id, user("the redb lock was the problem"))
            .unwrap();
        store
            .append_event(&b.id, user("something else entirely"))
            .unwrap();

        let report = Transcripts::new(&store)
            .search(&query("redb lock"))
            .unwrap();
        assert_eq!(report.total_matches, 1);
        assert_eq!(report.matched_conversations, 1);
        assert_eq!(report.hits[0].session_id, a.id);
        assert_eq!(report.hits[0].kind, kind::USER);
        assert!(report.hits[0].text.contains("redb lock"));
    }

    #[test]
    fn successful_tool_output_is_excluded_unless_asked_for_but_failures_never_are() {
        // The asymmetry is deliberate and is the one thing about this surface a
        // reader is likeliest to think is a bug, so it is pinned.
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();
        store
            .append_event(&s.id, tool_result("read_path", true, "fn widget() {}"))
            .unwrap();
        store
            .append_event(
                &s.id,
                tool_result("terminal_run", false, "widget: no such target"),
            )
            .unwrap();

        let t = Transcripts::new(&store);

        let quiet = t.search(&query("widget")).unwrap();
        assert_eq!(quiet.total_matches, 1, "only the failure should match");
        assert_eq!(quiet.hits[0].kind, kind::TOOL_FAILED);

        let loud = t
            .search(&SearchQuery {
                include_tool_output: true,
                ..query("widget")
            })
            .unwrap();
        assert_eq!(loud.total_matches, 2);
    }

    #[test]
    fn a_sub_agents_log_is_searchable_and_attributed() {
        let (store, _d) = temp_store();
        let parent = store
            .create_session(Some("parent".into()), &"agent", "local")
            .unwrap();
        let child = store.create_session(None, &"agent", "local").unwrap();
        crate::subagents::Subagents::new(&store)
            .register(
                &parent.id,
                &child.id,
                "scout",
                "go and look",
                "",
                "",
                "agent",
                0,
            )
            .unwrap();
        store
            .append_event(&child.id, assistant("the answer is fourteen"))
            .unwrap();

        let t = Transcripts::new(&store);

        // Excluded by default: a database has far more child sessions than
        // conversations, and unasked-for children would bury the answer.
        assert_eq!(t.search(&query("fourteen")).unwrap().total_matches, 0);

        let report = t
            .search(&SearchQuery {
                include_subagents: true,
                ..query("fourteen")
            })
            .unwrap();
        assert_eq!(report.total_matches, 1);
        let hit = &report.hits[0];
        assert!(hit.is_subagent);
        assert_eq!(hit.label, "scout", "a hit in a child names the child");
        assert_eq!(hit.session_id, child.id);
    }

    #[test]
    fn subagents_lists_a_whole_tree_for_any_conversation() {
        let (store, _d) = temp_store();
        let parent = store.create_session(None, &"agent", "local").unwrap();
        let child = store.create_session(None, &"agent", "local").unwrap();
        crate::subagents::Subagents::new(&store)
            .register(&parent.id, &child.id, "scout", "look", "", "", "plan", 0)
            .unwrap();

        let listed = Transcripts::new(&store).subagents(&parent.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, child.id);
        assert_eq!(listed[0].label, "scout");
        assert_eq!(listed[0].state, "running");
        assert_eq!(listed[0].task, "look");
    }

    #[test]
    fn a_read_pages_by_seq_and_reports_what_it_clipped() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();
        store.append_event(&s.id, user("first")).unwrap();
        store.append_event(&s.id, user(&"x".repeat(50))).unwrap();

        let t = Transcripts::new(&store);
        let all = t.read(&s.id, 0, 0, 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "first");
        assert_eq!(all[0].elided, 0);

        // from_seq is exclusive, so paging is `from_seq = last seq seen`.
        let tail = t.read(&s.id, all[0].seq, 0, 10).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text.chars().count(), 10);
        assert_eq!(tail[0].elided, 40);
    }

    #[test]
    fn transient_and_bookkeeping_events_say_nothing() {
        // These are never persisted, but the projection must agree: a stream
        // delta that rendered as a transcript line would double every
        // assistant message a search found.
        for event in [
            SessionEvent::StreamDelta("tok".into()),
            SessionEvent::ReasoningDelta("hmm".into()),
            SessionEvent::TurnStarted,
        ] {
            assert!(
                entries_of(&event, true).is_empty(),
                "{event:?} should be silent"
            );
        }
        // ...whereas a finished turn carries the stop reason, which is worth
        // grepping for.
        let finished = SessionEvent::TurnFinished(TurnStats {
            iterations: 3,
            prompt_tokens: 1,
            completion_tokens: 1,
            cost_usd: 0.5,
            tools_used: vec![],
            stopped_by: "max-iterations".into(),
        });
        let entries = entries_of(&finished, true);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].1.contains("max-iterations"));
    }

    #[test]
    fn a_tool_call_is_searchable_by_name_and_by_argument() {
        let event = SessionEvent::AssistantMessage(AssistantMsg {
            content: "looking now".into(),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "search_files".into(),
                arguments_json: r#"{"pattern":"scope_ok"}"#.into(),
            }],
            model: "test".into(),
            usage: None,
        });
        let entries = entries_of(&event, true);
        assert_eq!(
            entries.len(),
            2,
            "the message and the call are separate lines"
        );
        assert_eq!(entries[1].0, kind::TOOL_CALL);
        assert!(entries[1].1.contains("search_files"));
        assert!(entries[1].1.contains("scope_ok"));
    }

    #[test]
    fn an_image_only_message_is_still_findable_by_attachment_name() {
        let event = SessionEvent::UserMessage(UserMsg {
            text: String::new(),
            attachments: vec![Attachment {
                name: "screenshot.png".into(),
                mime: "image/png".into(),
                data_base64: "AAA".into(),
            }],
        });
        let entries = entries_of(&event, true);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].1.contains("screenshot.png"));
        // The base64 payload must never become searchable text: it would be
        // megabytes of noise that matches almost any short pattern.
        assert!(!entries[0].1.contains("AAA"));
    }

    #[test]
    fn the_hit_cap_is_reported_rather_than_hidden() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();
        for _ in 0..10 {
            store.append_event(&s.id, user("needle")).unwrap();
        }
        let report = Transcripts::new(&store)
            .search(&SearchQuery {
                max_results: 3,
                ..query("needle")
            })
            .unwrap();
        assert_eq!(report.hits.len(), 3);
        assert_eq!(report.total_matches, 10, "the tally counts past the cap");
        assert!(report.capped);
        assert!(report.incomplete);
    }

    #[test]
    fn a_bad_pattern_and_an_empty_one_are_refused() {
        let (store, _d) = temp_store();
        let t = Transcripts::new(&store);
        assert!(t.search(&query("[unclosed")).is_err());
        assert!(t.search(&query("   ")).is_err());
    }

    #[test]
    fn an_unknown_conversation_is_an_error_not_an_empty_transcript() {
        // Silence and absence must not look alike: "that conversation has
        // nothing in it" is a finding, and a wrong id is a mistake.
        let (store, _d) = temp_store();
        let t = Transcripts::new(&store);
        assert!(t.read("no-such-id", 0, 0, 0).is_err());
        assert!(
            t.search(&SearchQuery {
                session_id: "no-such-id".into(),
                ..query("x")
            })
            .is_err()
        );
    }

    #[test]
    fn conversations_are_newest_first_and_archived_ones_are_opt_in() {
        let (store, _d) = temp_store();
        let kept = store
            .create_session(Some("kept".into()), &"agent", "local")
            .unwrap();
        let filed = store
            .create_session(Some("filed".into()), &"agent", "local")
            .unwrap();
        store.append_event(&filed.id, user("a")).unwrap();
        store.append_event(&kept.id, user("b")).unwrap();
        store.archive_session(&filed.id, true).unwrap();

        let t = Transcripts::new(&store);

        let visible = t.conversations(false, false, 0).unwrap();
        assert_eq!(visible.len(), 1, "an archived conversation is opt-in");
        assert_eq!(visible[0].id, kept.id);

        let all = t.conversations(true, false, 0).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|c| !c.is_subagent));
        // Asserted as the ordering invariant rather than by naming which id
        // comes first: `updated_ms` has millisecond resolution and
        // `archive_session` bumps it, so any test that hard-codes a winner is
        // really testing the clock. Descending order is the actual contract —
        // it is what makes a capped scan hold the most recent conversations.
        assert!(
            all.windows(2).all(|w| w[0].updated_ms >= w[1].updated_ms),
            "conversations must come back most recently active first"
        );
    }

    #[test]
    fn a_limit_of_zero_means_everything() {
        let (store, _d) = temp_store();
        for _ in 0..3 {
            store.create_session(None, &"agent", "local").unwrap();
        }
        let t = Transcripts::new(&store);
        assert_eq!(t.conversations(false, false, 0).unwrap().len(), 3);
        assert_eq!(t.conversations(false, false, 2).unwrap().len(), 2);
    }

    #[test]
    fn nothing_here_can_write() {
        // The justification for reading every conversation is that reading is
        // all this module does. Checked textually, because the guarantee is
        // about what the code contains rather than what one call did: a future
        // `append` or `put_` here would widen a boundary that was only opened
        // on the promise it stays read-only.
        let src = include_str!("transcripts.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "begin_write",
            "append_event",
            "put_subagent",
            "kv_put",
            "rename_session",
            "archive_session",
        ] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` appears in transcripts.rs, which is allowed to read \
                 every conversation precisely because it cannot change any of them. \
                 Put the write somewhere that is scoped to its own session."
            );
        }
    }
}
