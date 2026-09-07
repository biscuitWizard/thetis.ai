//! Durable state: sessions, their append-only event logs, agent scratch KV,
//! spend accounting, and (from M2) the revision registry.
//!
//! Session state lives here rather than inside the agent so that guest
//! instances stay disposable: a crash, a hot swap, or an orchestrator restart
//! loses nothing.

use anyhow::{anyhow, Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::branches::BranchRow;
use crate::bindings::types::{
    EventRecord, SessionEvent, SessionMeta, ToolCall, ToolOutcome, TurnStats, UserMsg,
};

/// A session whose turn was cut short, and whether it should carry on.
#[derive(Debug, Clone, PartialEq)]
pub struct Interrupted {
    pub session_id: String,
    pub resume: bool,
}

/// Set while a restart is pending that the agent asked not to resume.
const NO_RESUME_KEY: &str = "__no_resume";
/// How many times running this turn has ended in an interruption.
const RESUME_ATTEMPTS_KEY: &str = "__resume_attempts";
/// Set just before a restart the system asked for, and cleared when
/// reconciliation sees it. An interruption the orchestrator caused on purpose
/// is not evidence that the turn is unhealthy.
const EXPECTED_RESTART_KEY: &str = "__expected_restart";
/// The event the log had reached when this turn was last interrupted, so the
/// next interruption can tell "it died again having got nowhere" from "it has
/// been working since and was interrupted again".
const RESUME_MARK_KEY: &str = "__resume_mark";
const MAX_RESUME_ATTEMPTS: u32 = 2;

/// session id -> SessionMeta (json)
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
/// (session id, seq) -> EventRecord (json)
const EVENTS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("events");
/// (scope, key) -> value; scope is "global" or a session id
const KV: TableDefinition<(&str, &str), &str> = TableDefinition::new("kv");
/// session id -> cumulative USD spend
const SPEND: TableDefinition<&str, f64> = TableDefinition::new("spend");
/// (aspect key, revision) -> RevisionRow (json)
pub(crate) const REVISIONS: TableDefinition<(&str, u64), &[u8]> =
    TableDefinition::new("revisions");
/// snapshot id -> SystemSnapshot (json)
pub(crate) const SNAPSHOTS: TableDefinition<u64, &[u8]> = TableDefinition::new("snapshots");
/// "model|dims|content hash" -> little-endian f32 embedding of a skill card
const SKILL_VECTORS: TableDefinition<&str, &[u8]> = TableDefinition::new("skill_vectors");
/// session id -> BranchRow (json): the sandbox branch backing a conversation
const BRANCHES: TableDefinition<&str, &[u8]> = TableDefinition::new("branches");

/// The title a conversation starts with, and the only one auto-titling will
/// overwrite.
pub const DEFAULT_TITLE: &str = "New chat";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
        let db = Database::create(path)
            .with_context(|| format!("opening database {}", path.display()))?;

        // Create every table up front so read transactions never hit a missing
        // table on a fresh database.
        let txn = db.begin_write()?;
        {
            txn.open_table(SESSIONS)?;
            txn.open_table(EVENTS)?;
            txn.open_table(KV)?;
            txn.open_table(SPEND)?;
            txn.open_table(REVISIONS)?;
            txn.open_table(SNAPSHOTS)?;
            txn.open_table(SKILL_VECTORS)?;
            txn.open_table(BRANCHES)?;
        }
        txn.commit()?;

        Ok(Self { db })
    }

    // --- branches ------------------------------------------------------------

    pub fn put_branch(&self, row: &BranchRow) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(BRANCHES)?;
            t.insert(row.session_id.as_str(), serde_json::to_vec(row)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_branch(&self, session_id: &str) -> Result<Option<BranchRow>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(BRANCHES)?;
        match t.get(session_id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_branches(&self) -> Result<Vec<BranchRow>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(BRANCHES)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (_, v) = entry?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    pub fn remove_branch(&self, session_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(BRANCHES)?;
            t.remove(session_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    // --- skill vectors -----------------------------------------------------

    /// A cached embedding, or `None` when it has not been computed under this
    /// key. A read failure is reported as a miss: re-embedding costs a little
    /// money, whereas failing a turn costs the conversation.
    pub fn skill_vector(&self, key: &str) -> Option<Vec<u8>> {
        let read = || -> Result<Option<Vec<u8>>> {
            let txn = self.db.begin_read()?;
            let t = txn.open_table(SKILL_VECTORS)?;
            Ok(t.get(key)?.map(|v| v.value().to_vec()))
        };
        match read() {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, "reading a cached skill vector failed");
                None
            }
        }
    }

    pub fn put_skill_vector(&self, key: &str, vector: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(SKILL_VECTORS)?;
            t.insert(key, vector)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Deletes every cached vector whose key is not in `keep`, returning how
    /// many went. Called after a rescan so renamed or deleted skills, and
    /// vectors from a previous embedding model, do not accumulate forever.
    pub fn retain_skill_vectors(&self, keep: &[String]) -> Result<usize> {
        let keep: std::collections::HashSet<&str> = keep.iter().map(|s| s.as_str()).collect();

        let txn = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut t = txn.open_table(SKILL_VECTORS)?;
            // Collected first: the iterator borrows the table that the removal
            // needs mutably.
            let stale: Vec<String> = t
                .iter()?
                .filter_map(|row| row.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| !keep.contains(k.as_str()))
                .collect();
            for key in stale {
                t.remove(key.as_str())?;
                removed += 1;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    // --- sessions ----------------------------------------------------------

    pub fn create_session(&self, title: Option<String>, mode: &str) -> Result<SessionMeta> {
        let now = now_ms();
        let meta = SessionMeta {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.unwrap_or_else(|| DEFAULT_TITLE.to_string()),
            created_ms: now,
            updated_ms: now,
            event_count: 0,
            archived: false,
            preview: String::new(),
            mode: mode.to_string(),
            model: String::new(),
        };
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(SESSIONS)?;
            t.insert(meta.id.as_str(), serde_json::to_vec(&meta)?.as_slice())?;
        }
        txn.commit()?;
        Ok(meta)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionMeta>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SESSIONS)?;
        match t.get(id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionMeta>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SESSIONS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            let meta: SessionMeta = serde_json::from_slice(v.value())?;
            if include_archived || !meta.archived {
                out.push(meta);
            }
        }
        // Most recently active first — the order the sidebar wants.
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    fn update_meta<F>(&self, id: &str, f: F) -> Result<SessionMeta>
    where
        F: FnOnce(&mut SessionMeta),
    {
        let txn = self.db.begin_write()?;
        let meta = {
            let mut t = txn.open_table(SESSIONS)?;
            let mut meta: SessionMeta = match t.get(id)? {
                Some(v) => serde_json::from_slice(v.value())?,
                None => return Err(anyhow!("no such session: {id}")),
            };
            f(&mut meta);
            t.insert(id, serde_json::to_vec(&meta)?.as_slice())?;
            meta
        };
        txn.commit()?;
        Ok(meta)
    }

    pub fn rename_session(&self, id: &str, title: &str) -> Result<SessionMeta> {
        let title = title.trim().chars().take(120).collect::<String>();
        self.update_meta(id, |m| {
            m.title = title;
            m.updated_ms = now_ms();
        })
    }

    pub fn archive_session(&self, id: &str, archived: bool) -> Result<SessionMeta> {
        self.update_meta(id, |m| {
            m.archived = archived;
            m.updated_ms = now_ms();
        })
    }

    pub fn set_mode(&self, id: &str, mode: &str) -> Result<SessionMeta> {
        let mode = mode.trim().chars().take(32).collect::<String>();
        self.update_meta(id, |m| m.mode = mode)
    }

    pub fn set_model(&self, id: &str, model: &str) -> Result<SessionMeta> {
        let model = model.trim().chars().take(128).collect::<String>();
        self.update_meta(id, |m| m.model = model)
    }

    // --- events ------------------------------------------------------------

    /// Appends one event and returns it stamped with its sequence number.
    ///
    /// Sequence allocation and the metadata update share a single write
    /// transaction, so concurrent appends can never produce a duplicate seq.
    pub fn append_event(&self, session_id: &str, event: SessionEvent) -> Result<EventRecord> {
        let ts = now_ms();
        let txn = self.db.begin_write()?;
        let record = {
            let mut sessions = txn.open_table(SESSIONS)?;
            let mut meta: SessionMeta = match sessions.get(session_id)? {
                Some(v) => serde_json::from_slice(v.value())?,
                None => return Err(anyhow!("no such session: {session_id}")),
            };

            let seq = meta.event_count + 1;
            meta.event_count = seq;
            meta.updated_ms = ts;
            if let Some(p) = preview_of(&event) {
                meta.preview = p.chars().take(140).collect();
            }
            // Name the conversation after whatever opened it, so the sidebar is
            // not a column of identical placeholders. Only ever applies while
            // the title is still the untouched default.
            if meta.title == DEFAULT_TITLE {
                if let SessionEvent::UserMessage(msg) = &event {
                    if let Some(title) = derive_title(msg) {
                        meta.title = title;
                    }
                }
            }

            let record = EventRecord { seq, ts_ms: ts, event };
            let mut events = txn.open_table(EVENTS)?;
            events.insert(
                (session_id, seq),
                serde_json::to_vec(&record)?.as_slice(),
            )?;
            sessions.insert(session_id, serde_json::to_vec(&meta)?.as_slice())?;
            record
        };
        txn.commit()?;
        Ok(record)
    }

    /// All events with `seq > from_seq`, in order.
    pub fn events(&self, session_id: &str, from_seq: u64) -> Result<Vec<EventRecord>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(EVENTS)?;
        let start = (session_id, from_seq.saturating_add(1));
        let end = (session_id, u64::MAX);
        let mut out = Vec::new();
        for row in t.range(start..=end)? {
            let (_, v) = row?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    /// Repairs the log of any turn that was cut short and reports which
    /// sessions should pick up where they left off.
    ///
    /// A turn killed mid-flight — by a restart or a crash — leaves a
    /// `turn-started` with nothing after it, and possibly tool calls whose
    /// results never arrived. Both have to be dealt with before the turn can
    /// resume: a model request carrying tool calls with no matching results is
    /// rejected outright by most providers, so the log is made coherent first.
    /// Repairs interrupted turns, optionally for one session only.
    ///
    /// `only` matters when several conversations are running: a worker dying
    /// is news about *that* conversation, and sweeping the whole fleet on
    /// every death drags in sessions that are merely between workers — an
    /// agent mid-restart looks exactly like a crashed one from out here. Three
    /// self-modifying agents turned that into a resume storm, each death
    /// resuming the others and spending their attempt budgets until every
    /// conversation was abandoned. Only the boot sweep wants the whole fleet.
    pub fn reconcile_interrupted_turns(
        &self,
        note: &str,
        skip: &[String],
        only: Option<&str>,
    ) -> Result<Vec<Interrupted>> {
        let mut found = Vec::new();

        for meta in self.list_sessions(true)? {
            if only.is_some_and(|id| id != meta.id) {
                continue;
            }
            // A session with a live worker is not interrupted, whatever its
            // log looks like mid-turn: a dangling turn-started and unanswered
            // tool calls are exactly what a *running* turn looks like from
            // outside. Synthesizing results for those once raced a real
            // result into the log and bricked the conversation.
            if skip.contains(&meta.id) {
                continue;
            }
            let events = self.events(&meta.id, 0)?;

            let last_marker = events.iter().rev().find(|r| {
                matches!(
                    r.event,
                    SessionEvent::TurnStarted
                        | SessionEvent::TurnFinished(_)
                        | SessionEvent::Incident(_)
                )
            });
            if !matches!(last_marker.map(|r| &r.event), Some(SessionEvent::TurnStarted)) {
                continue;
            }

            for call in unanswered_tool_calls(&events) {
                self.append_event(
                    &meta.id,
                    SessionEvent::ToolResult(ToolOutcome {
                        call_id: call.id,
                        name: call.name,
                        ok: false,
                        content: "Interrupted: Thetis restarted before this finished.                                   Run it again if the result still matters."
                            .to_string(),
                    }),
                )?;
            }

            // A note rather than an incident: the turn is continuing, and the
            // model reads this as context for why its last step went missing.
            self.append_event(&meta.id, SessionEvent::SystemNote(note.to_string()))?;

            // Clearing a flag writes an empty value rather than removing the
            // row, so presence alone does not mean set.
            let opted_out = self
                .kv_get(&meta.id, NO_RESUME_KEY)?
                .is_some_and(|v| !v.is_empty());
            let attempts = self
                .kv_get(&meta.id, RESUME_ATTEMPTS_KEY)?
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);

            // Did we ask for this? Adopting a rebuilt kernel means stopping
            // the worker mid-turn on purpose, and that is the supported way to
            // pick up your own changes — several agents do it repeatedly in one
            // turn. Counting it as a death spent the whole budget on healthy
            // work and abandoned the turn with "interrupted 2 times".
            //
            // The guard itself dates from the single-process orchestrator,
            // where a turn that died really did take everything with it. With
            // a worker per conversation a dying turn costs only its own worker,
            // so the budget is only meant for a turn that crashes it *by
            // itself*, repeatedly.
            let expected = self
                .kv_get(&meta.id, EXPECTED_RESTART_KEY)?
                .is_some_and(|v| !v.is_empty());
            if expected {
                self.kv_put(&meta.id, EXPECTED_RESTART_KEY, "")?;
            }

            // Has the turn got anywhere since it was last interrupted?
            //
            // The budget exists to stop a turn that cannot survive being
            // resumed — one that dies immediately, every time, making no
            // progress. It was being spent by turns that were working
            // perfectly well and merely got interrupted repeatedly, which is a
            // problem with whatever is interrupting them, not with the turn.
            // Counting only fruitless attempts keeps the crash-loop guard
            // while letting a healthy turn be picked up as often as it needs.
            let mark = self
                .kv_get(&meta.id, RESUME_MARK_KEY)?
                .and_then(|v| v.parse::<u64>().ok());
            let progressed = match mark {
                None => false,
                Some(mark) => events.iter().any(|r| {
                    r.seq > mark
                        && matches!(
                            r.event,
                            SessionEvent::AssistantMessage(_)
                                | SessionEvent::ToolInvocation(_)
                                | SessionEvent::ToolResult(_)
                        )
                }),
            };
            let attempts = if progressed { 0 } else { attempts };
            if progressed {
                self.clear_resume_attempts(&meta.id)?;
            }
            let latest = events.last().map(|r| r.seq).unwrap_or(0);
            self.kv_put(&meta.id, RESUME_MARK_KEY, &latest.to_string())?;

            // A turn that keeps dying takes its worker with it each time.
            // Stop after a couple of tries rather than looping forever.
            let exhausted = !expected && attempts >= MAX_RESUME_ATTEMPTS;
            let resume = !opted_out && !exhausted;

            if opted_out {
                self.kv_put(&meta.id, NO_RESUME_KEY, "")?;
            }
            if exhausted {
                self.append_event(
                    &meta.id,
                    SessionEvent::Incident(format!(
                        "This turn has been interrupted {attempts} times; not resuming it again."
                    )),
                )?;
                self.clear_resume_attempts(&meta.id)?;
            } else if resume && !expected {
                self.kv_put(&meta.id, RESUME_ATTEMPTS_KEY, &(attempts + 1).to_string())?;
            } else {
                // Deliberately not carrying on, so the turn has to be closed:
                // an unmatched `turn-started` reads as a conversation still
                // thinking, and nothing would ever clear it.
                self.append_event(
                    &meta.id,
                    SessionEvent::TurnFinished(TurnStats {
                        iterations: 0,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        cost_usd: 0.0,
                        tools_used: Vec::new(),
                        stopped_by: "restarted".to_string(),
                    }),
                )?;
            }

            found.push(Interrupted {
                session_id: meta.id,
                resume,
            });
        }

        Ok(found)
    }

    /// Called once a turn ends normally, so a later interruption starts from a
    /// clean count.
    pub fn clear_resume_attempts(&self, session_id: &str) -> Result<()> {
        self.kv_put(session_id, RESUME_ATTEMPTS_KEY, "")
    }

    /// Records that the next restart of this session should not resume its turn.
    /// Declares that this session is about to be interrupted on purpose, so
    /// the next reconciliation does not charge it an attempt.
    pub fn expect_restart(&self, session_id: &str) -> Result<()> {
        self.kv_put(session_id, EXPECTED_RESTART_KEY, "1")
    }

    pub fn set_no_resume(&self, session_id: &str, no_resume: bool) -> Result<()> {
        self.kv_put(session_id, NO_RESUME_KEY, if no_resume { "1" } else { "" })
    }

    // --- kv ----------------------------------------------------------------

    pub fn kv_get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(KV)?;
        Ok(t.get((scope, key))?.map(|v| v.value().to_string()))
    }

    pub fn kv_put(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(KV)?;
            t.insert((scope, key), value)?;
        }
        txn.commit()?;
        Ok(())
    }

    // --- spend accounting --------------------------------------------------

    pub fn get_spend(&self, session_id: &str) -> Result<f64> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SPEND)?;
        Ok(t.get(session_id)?.map(|v| v.value()).unwrap_or(0.0))
    }

    pub fn add_spend(&self, session_id: &str, usd: f64) -> Result<f64> {
        let txn = self.db.begin_write()?;
        let total = {
            let mut t = txn.open_table(SPEND)?;
            let total = t.get(session_id)?.map(|v| v.value()).unwrap_or(0.0) + usd;
            t.insert(session_id, total)?;
            total
        };
        txn.commit()?;
        Ok(total)
    }
}

// --- revisions & system snapshots -------------------------------------------

impl Store {
    /// Next unused revision number for an aspect. Revisions never restart, even
    /// after a rollback, so history is always append-only.
    pub fn next_revision(&self, aspect_key: &str) -> Result<u64> {
        let rows: Vec<crate::revisions::RevisionRow> = self.list_revisions(aspect_key)?;
        Ok(rows.last().map(|r| r.revision).unwrap_or(0) + 1)
    }

    pub fn put_revision<T: serde::Serialize>(&self, aspect_key: &str, revision: u64, row: &T) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(REVISIONS)?;
            t.insert((aspect_key, revision), serde_json::to_vec(row)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_revision<T: serde::de::DeserializeOwned>(
        &self,
        aspect_key: &str,
        revision: u64,
    ) -> Result<Option<T>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(REVISIONS)?;
        match t.get((aspect_key, revision))? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_revisions<T: serde::de::DeserializeOwned>(&self, aspect_key: &str) -> Result<Vec<T>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(REVISIONS)?;
        let mut out = Vec::new();
        for row in t.range((aspect_key, 0u64)..=(aspect_key, u64::MAX))? {
            let (_, v) = row?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    pub fn put_snapshot<T: serde::Serialize>(&self, id: u64, snapshot: &T) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(SNAPSHOTS)?;
            t.insert(id, serde_json::to_vec(snapshot)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn list_snapshots<T: serde::de::DeserializeOwned>(&self) -> Result<Vec<T>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SNAPSHOTS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    pub fn next_snapshot_id(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SNAPSHOTS)?;
        let next = match t.last()? {
            Some((key, _)) => key.value() + 1,
            None => 1,
        };
        Ok(next)
    }

    pub fn get_snapshot<T: serde::de::DeserializeOwned>(&self, id: u64) -> Result<Option<T>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SNAPSHOTS)?;
        match t.get(id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }
}

/// Tool calls from the turn's last assistant message that never got a result.
///
/// Pairing is done against the assistant message rather than the
/// `tool-invocation` events, because a turn can die between announcing a call
/// and recording that it started — leaving a call the provider expects an
/// answer for and no trace that it was ever dispatched.
fn unanswered_tool_calls(events: &[EventRecord]) -> Vec<ToolCall> {
    let Some(last_assistant) = events
        .iter()
        .rposition(|r| matches!(r.event, SessionEvent::AssistantMessage(_)))
    else {
        return Vec::new();
    };

    let SessionEvent::AssistantMessage(msg) = &events[last_assistant].event else {
        return Vec::new();
    };
    if msg.tool_calls.is_empty() {
        return Vec::new();
    }

    let answered: std::collections::HashSet<&str> = events[last_assistant + 1..]
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::ToolResult(out) => Some(out.call_id.as_str()),
            _ => None,
        })
        .collect();

    msg.tool_calls
        .iter()
        .filter(|c| !answered.contains(c.id.as_str()))
        .cloned()
        .collect()
}

/// A conversation title taken from its opening message.
fn derive_title(msg: &UserMsg) -> Option<String> {
    // Collapse newlines and runs of spaces: titles are one line.
    let text = msg.text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !text.is_empty() {
        return Some(shorten(&text, 48));
    }

    // Nothing was typed, so name it after what was attached.
    match msg.attachments.len() {
        0 => None,
        1 => Some(shorten(&msg.attachments[0].name, 48)),
        n => Some(format!("{n} images")),
    }
}

/// Truncates on a word boundary where one is close enough to the limit.
fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max_chars).collect();
    let cut = match clipped.rfind(' ') {
        // Only break at a space if it leaves a title worth reading; otherwise
        // a single long word would collapse to almost nothing.
        Some(idx) if idx >= max_chars / 2 => &clipped[..idx],
        _ => clipped.as_str(),
    };
    format!("{}…", cut.trim_end())
}

/// Text worth showing in the session list for this event, if any.
fn preview_of(event: &SessionEvent) -> Option<String> {
    match event {
        SessionEvent::UserMessage(msg) => Some(if msg.text.trim().is_empty() {
            // An image-only message still deserves a recognisable preview.
            match msg.attachments.len() {
                0 => String::new(),
                1 => format!("[{}]", msg.attachments[0].name),
                n => format!("[{n} attachments]"),
            }
        } else {
            msg.text.clone()
        }),
        SessionEvent::AssistantMessage(m) if !m.content.trim().is_empty() => {
            Some(m.content.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
use crate::bindings::types::{AssistantMsg, Attachment, ToolCall, ToolOutcome};

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMsg {
            text: text.to_string(),
            attachments: vec![],
        })
    }

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        (store, dir)
    }

    #[test]
    fn archiving_only_sets_a_flag_and_get_session_still_returns_it() {
        // The property the Discord connector depends on: archiving is not a
        // delete, so checking that a mapped session merely *exists* would let a
        // chat surface carry on in an archived conversation. Anything reusing a
        // session id has to read `archived`.
        let (store, _d) = temp_store();
        let s = store.create_session(None, "chat").unwrap();
        assert!(!s.archived);

        store.archive_session(&s.id, true).unwrap();

        let found = store.get_session(&s.id).unwrap().expect("still present");
        assert!(found.archived, "archiving must set the flag");
        assert!(
            !store.list_sessions(false).unwrap().iter().any(|m| m.id == s.id),
            "an archived session is hidden from the default listing"
        );
        assert!(
            store.list_sessions(true).unwrap().iter().any(|m| m.id == s.id),
            "and still available when archived ones are asked for"
        );
    }

    #[test]
    fn events_are_sequential_and_readable_from_offset() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();

        for i in 0..5 {
            let rec = store
                .append_event(&s.id, user(&format!("msg {i}")))
                .unwrap();
            assert_eq!(rec.seq, i + 1);
        }

        let all = store.events(&s.id, 0).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].seq, 1);

        let tail = store.events(&s.id, 3).unwrap();
        assert_eq!(tail.len(), 2, "from_seq is exclusive");
        assert_eq!(tail[0].seq, 4);
    }

    #[test]
    fn events_are_isolated_per_session() {
        let (store, _d) = temp_store();
        let a = store.create_session(Some("a".into()), "agent").unwrap();
        let b = store.create_session(Some("b".into()), "agent").unwrap();

        store
            .append_event(&a.id, user("in a"))
            .unwrap();
        store
            .append_event(&b.id, user("in b"))
            .unwrap();

        assert_eq!(store.events(&a.id, 0).unwrap().len(), 1);
        assert_eq!(store.events(&b.id, 0).unwrap().len(), 1);
    }

    #[test]
    fn preview_tracks_conversation_not_bookkeeping() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();

        store
            .append_event(&s.id, user("hello there"))
            .unwrap();
        store
            .append_event(&s.id, SessionEvent::TurnStarted)
            .unwrap();

        let meta = store.get_session(&s.id).unwrap().unwrap();
        assert_eq!(meta.preview, "hello there", "TurnStarted must not clobber");
        assert_eq!(meta.event_count, 2);

        store
            .append_event(
                &s.id,
                SessionEvent::AssistantMessage(AssistantMsg {
                    content: "general kenobi".into(),
                    tool_calls: vec![],
                    model: "m".into(),
                    usage: None,
                }),
            )
            .unwrap();
        let meta = store.get_session(&s.id).unwrap().unwrap();
        assert_eq!(meta.preview, "general kenobi");
    }

    #[test]
    fn first_message_names_the_conversation() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();
        assert_eq!(s.title, DEFAULT_TITLE);

        store
            .append_event(&s.id, user("How do I add a tool to this thing?"))
            .unwrap();
        let titled = store.get_session(&s.id).unwrap().unwrap();
        assert_eq!(titled.title, "How do I add a tool to this thing?");

        // Later messages must not rename a conversation out from under the user.
        store.append_event(&s.id, user("second message")).unwrap();
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().title,
            "How do I add a tool to this thing?"
        );
    }

    #[test]
    fn auto_titles_are_shortened_on_a_word_boundary() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();

        store
            .append_event(
                &s.id,
                user("please explain in detail how the revision registry decides what counts as a known good build"),
            )
            .unwrap();

        let title = store.get_session(&s.id).unwrap().unwrap().title;
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= 49, "{title}");
        assert!(!title.contains("  "), "newlines and runs of spaces collapse");
        // The cut lands between words, not inside one.
        assert!(title.starts_with("please explain in detail how the revision"), "{title}");
    }

    #[test]
    fn a_renamed_conversation_is_never_auto_titled() {
        let (store, _d) = temp_store();
        let s = store.create_session(Some("Budget review".into()), "agent").unwrap();

        store.append_event(&s.id, user("hello there")).unwrap();

        assert_eq!(store.get_session(&s.id).unwrap().unwrap().title, "Budget review");
    }

    #[test]
    fn image_only_conversations_are_named_after_the_file() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();

        store
            .append_event(
                &s.id,
                SessionEvent::UserMessage(UserMsg {
                    text: String::new(),
                    attachments: vec![Attachment {
                        name: "receipt.png".into(),
                        mime: "image/png".into(),
                        data_base64: "iVBORw0KGgo=".into(),
                    }],
                }),
            )
            .unwrap();

        assert_eq!(store.get_session(&s.id).unwrap().unwrap().title, "receipt.png");
    }

    #[test]
    fn session_mode_and_model_round_trip() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();
        assert_eq!(s.mode, "agent");
        assert_eq!(s.model, "", "no override until one is chosen");

        store.set_mode(&s.id, "plan").unwrap();
        store.set_model(&s.id, "mock/echo").unwrap();

        let reloaded = store.get_session(&s.id).unwrap().unwrap();
        assert_eq!(reloaded.mode, "plan");
        assert_eq!(reloaded.model, "mock/echo");

        // Clearing the override falls back to the grip default.
        store.set_model(&s.id, "").unwrap();
        assert_eq!(store.get_session(&s.id).unwrap().unwrap().model, "");
    }

    #[test]
    fn image_only_messages_get_a_readable_preview() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();

        let image = Attachment {
            name: "chart.png".into(),
            mime: "image/png".into(),
            data_base64: "iVBORw0KGgo=".into(),
        };
        store
            .append_event(
                &s.id,
                SessionEvent::UserMessage(UserMsg {
                    text: String::new(),
                    attachments: vec![image.clone()],
                }),
            )
            .unwrap();
        assert_eq!(store.get_session(&s.id).unwrap().unwrap().preview, "[chart.png]");

        store
            .append_event(
                &s.id,
                SessionEvent::UserMessage(UserMsg {
                    text: String::new(),
                    attachments: vec![image.clone(), image],
                }),
            )
            .unwrap();
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().preview,
            "[2 attachments]"
        );
    }

    /// Builds a session sitting mid-turn, optionally with a tool call that
    /// never got its result.
    fn mid_turn(store: &Store, title: &str, pending_call: Option<&str>) -> String {
        let s = store.create_session(Some(title.into()), "agent").unwrap();
        store.append_event(&s.id, user("do the thing")).unwrap();
        store.append_event(&s.id, SessionEvent::TurnStarted).unwrap();

        if let Some(call_id) = pending_call {
            store
                .append_event(
                    &s.id,
                    SessionEvent::AssistantMessage(AssistantMsg {
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: call_id.to_string(),
                            name: "terminal_run".into(),
                            arguments_json: "{}".into(),
                        }],
                        model: "m".into(),
                        usage: None,
                    }),
                )
                .unwrap();
        }
        s.id
    }

    fn finish_turn(store: &Store, id: &str) {
        store
            .append_event(
                id,
                SessionEvent::TurnFinished(crate::bindings::types::TurnStats {
                    iterations: 1,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cost_usd: 0.0,
                    tools_used: vec![],
                    stopped_by: "stop".into(),
                }),
            )
            .unwrap();
    }

    #[test]
    fn only_turns_left_mid_flight_are_reconciled() {
        let (store, _d) = temp_store();

        let done = mid_turn(&store, "finished", None);
        finish_turn(&store, &done);
        let cut_short = mid_turn(&store, "interrupted", None);

        let found = store.reconcile_interrupted_turns("restarted", &[], None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, cut_short);
        assert!(found[0].resume, "an interrupted turn resumes by default");

        // The completed conversation was left alone.
        let tail = store.events(&done, 0).unwrap();
        assert!(matches!(
            tail.last().map(|r| &r.event),
            Some(SessionEvent::TurnFinished(_))
        ));
    }

    #[test]
    fn a_tool_call_that_never_returned_gets_an_answer() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "mid tool call", Some("call_abc"));

        store.reconcile_interrupted_turns("restarted", &[], None).unwrap();

        // Providers reject a request whose tool calls have no matching results,
        // so the turn could not be resumed without this.
        let events = store.events(&id, 0).unwrap();
        let answered: Vec<&str> = events
            .iter()
            .filter_map(|r| match &r.event {
                SessionEvent::ToolResult(out) => Some(out.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(answered, vec!["call_abc"]);

        let result = events.iter().find_map(|r| match &r.event {
            SessionEvent::ToolResult(out) => Some(out),
            _ => None,
        });
        assert!(!result.unwrap().ok, "it did not succeed, and should say so");
    }

    #[test]
    fn a_call_that_did_return_is_not_answered_twice() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "answered", Some("call_ok"));
        store
            .append_event(
                &id,
                SessionEvent::ToolResult(ToolOutcome {
                    call_id: "call_ok".into(),
                    name: "terminal_run".into(),
                    ok: true,
                    content: "done".into(),
                }),
            )
            .unwrap();

        store.reconcile_interrupted_turns("restarted", &[], None).unwrap();

        let results = store
            .events(&id, 0)
            .unwrap()
            .iter()
            .filter(|r| matches!(r.event, SessionEvent::ToolResult(_)))
            .count();
        assert_eq!(results, 1);
    }

    #[test]
    fn a_restart_asked_not_to_resume_does_not() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "one-way", None);
        store.set_no_resume(&id, true).unwrap();

        let found = store.reconcile_interrupted_turns("restarted", &[], None).unwrap();
        assert_eq!(found.len(), 1);
        assert!(!found[0].resume);

        // The turn is closed rather than left looking like it is still running.
        assert!(matches!(
            store.events(&id, 0).unwrap().last().map(|r| &r.event),
            Some(SessionEvent::TurnFinished(_))
        ));

        // The preference is spent, so a later interruption resumes normally.
        store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        let again = store.reconcile_interrupted_turns("restarted", &[], None).unwrap();
        assert!(again[0].resume);
    }

    #[test]
    fn a_turn_that_keeps_dying_stops_being_resumed() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "doomed", None);

        // Each pass ends mid-turn again, as it would if resuming crashed.
        for expected in [true, true, false] {
            let found = store.reconcile_interrupted_turns("restarted", &[], None).unwrap();
            assert_eq!(found[0].resume, expected, "attempt outcome");
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        }

        assert!(store
            .events(&id, 0)
            .unwrap()
            .iter()
            .any(|r| matches!(&r.event, SessionEvent::Incident(t) if t.contains("not resuming"))));
    }

    #[test]
    fn a_turn_that_keeps_working_is_picked_up_as_often_as_it_needs() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "productive but interrupted", None);

        // What was actually happening in production: the turn was fine, the
        // worker under it kept dying, and after two deaths a working turn was
        // abandoned. Progress between interruptions means the turn survives
        // resuming, which is the only thing the budget is meant to test.
        for round in 0..8 {
            let found = store
                .reconcile_interrupted_turns("restarted", &[], None)
                .unwrap();
            assert!(found[0].resume, "round {round} must still resume");
            // It gets somewhere before the next interruption.
            store
                .append_event(
                    &id,
                    SessionEvent::AssistantMessage(AssistantMsg {
                        content: "got somewhere".into(),
                        tool_calls: Vec::new(),
                        model: "m".into(),
                        usage: None,
                    }),
                )
                .unwrap();
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        }
        assert!(
            !store
                .events(&id, 0)
                .unwrap()
                .iter()
                .any(|r| matches!(&r.event, SessionEvent::Incident(t) if t.contains("not resuming"))),
            "a turn making progress must never be abandoned"
        );
    }

    #[test]
    fn a_restart_we_asked_for_is_not_charged_against_the_turn() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "self-modifying", None);

        // Adopting a kernel you just built means restarting mid-turn, and an
        // agent iterating on the orchestrator does it repeatedly. Before this,
        // the third one hit the cap and the turn was abandoned for doing
        // exactly what the design intends.
        for round in 0..6 {
            store.expect_restart(&id).unwrap();
            let found = store
                .reconcile_interrupted_turns("restarted", &[], None)
                .unwrap();
            assert!(found[0].resume, "round {round} must still resume");
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        }
        assert!(
            !store
                .events(&id, 0)
                .unwrap()
                .iter()
                .any(|r| matches!(&r.event, SessionEvent::Incident(t) if t.contains("not resuming"))),
            "an expected restart must never exhaust the budget"
        );

        // But a genuine crash-loop is still caught: the marker is one-shot, so
        // once it is spent the ordinary budget applies again.
        for expected in [true, true, false] {
            let found = store
                .reconcile_interrupted_turns("restarted", &[], None)
                .unwrap();
            assert_eq!(found[0].resume, expected, "unexpected deaths still count");
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        }
    }

    #[test]
    fn one_conversation_dying_does_not_disturb_another() {
        let (store, _d) = temp_store();
        let dead = mid_turn(&store, "the one whose worker died", None);
        let busy = mid_turn(&store, "someone else mid-restart", None);

        // A worker death is news about its own conversation. Sweeping the
        // fleet dragged in every session that merely had no worker at that
        // instant, which with several self-modifying agents fed back on itself
        // until they had all spent their budgets.
        let found = store
            .reconcile_interrupted_turns("restarted", &[], Some(&dead))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, dead);

        let untouched = store.events(&busy, 0).unwrap();
        assert!(
            !untouched
                .iter()
                .any(|r| matches!(&r.event, SessionEvent::SystemNote(_))),
            "the other conversation must not even be annotated"
        );
    }

    #[test]
    fn finishing_a_turn_clears_the_attempt_count() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "recovers", None);

        store.reconcile_interrupted_turns("restarted", &[], None).unwrap();
        store.clear_resume_attempts(&id).unwrap();

        // Back to a clean slate: the cap starts counting from zero again.
        for _ in 0..2 {
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
            let found = store.reconcile_interrupted_turns("restarted", &[], None).unwrap();
            assert!(found[0].resume);
        }
    }

    #[test]
    fn spend_accumulates() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, "agent").unwrap();
        assert_eq!(store.get_spend(&s.id).unwrap(), 0.0);
        store.add_spend(&s.id, 0.25).unwrap();
        let total = store.add_spend(&s.id, 0.5).unwrap();
        assert!((total - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn kv_is_scoped() {
        let (store, _d) = temp_store();
        store.kv_put("global", "k", "g").unwrap();
        store.kv_put("sess-1", "k", "s").unwrap();
        assert_eq!(store.kv_get("global", "k").unwrap().as_deref(), Some("g"));
        assert_eq!(store.kv_get("sess-1", "k").unwrap().as_deref(), Some("s"));
        assert_eq!(store.kv_get("other", "k").unwrap(), None);
    }

    #[test]
    fn archived_sessions_are_filtered() {
        let (store, _d) = temp_store();
        let s = store.create_session(Some("keep".into()), "agent").unwrap();
        let g = store.create_session(Some("gone".into()), "agent").unwrap();
        store.archive_session(&g.id, true).unwrap();

        let visible = store.list_sessions(false).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, s.id);
        assert_eq!(store.list_sessions(true).unwrap().len(), 2);
    }
}
