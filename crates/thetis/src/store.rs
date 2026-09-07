//! Durable state: sessions, their append-only event logs, agent scratch KV,
//! spend accounting, and (from M2) the revision registry.
//!
//! Session state lives here rather than inside the agent so that guest
//! instances stay disposable: a crash, a hot swap, or an orchestrator restart
//! loses nothing.

use anyhow::{Context, Result, anyhow};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bindings::types::{
    EventRecord, SessionEvent, SessionMeta, ToolCall, ToolOutcome, TurnStats, UserMsg,
};
use crate::branches::BranchRow;
use crate::subagents::SubagentRow;

/// A session whose turn was cut short, and whether it should carry on.
#[derive(Debug, Clone, PartialEq)]
pub struct Interrupted {
    pub session_id: String,
    pub resume: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LoginRow {
    pub user_id: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
    pub expires_ms: u64,
    pub user_agent: String,
}

/// How far a session has got, read while its turn is still running.
///
/// Deliberately three numbers and no interpretation: a caller that wants to
/// say "busy" or "stuck" has the elapsed time to compare them against, and the
/// judgement belongs where the words are written rather than here.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionProgress {
    /// USD recorded against this session by the spend ledger so far.
    pub cost_usd: f64,
    /// Events in its log.
    pub events: u64,
    /// When it last appended one. 0 for a session with no log at all.
    pub activity_ms: u64,
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
/// top-level session id -> owning principal id
const OWNERS: TableDefinition<&str, &str> = TableDefinition::new("owners");
/// sha256(login token) -> LoginRow JSON
const LOGINS: TableDefinition<&str, &[u8]> = TableDefinition::new("logins");
/// principal id -> cumulative USD spend
const USER_SPEND: TableDefinition<&str, f64> = TableDefinition::new("user_spend");
/// (aspect key, revision) -> RevisionRow (json)
pub(crate) const REVISIONS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("revisions");
/// snapshot id -> SystemSnapshot (json)
pub(crate) const SNAPSHOTS: TableDefinition<u64, &[u8]> = TableDefinition::new("snapshots");
/// "model|dims|content hash" -> little-endian f32 embedding of a skill card
const SKILL_VECTORS: TableDefinition<&str, &[u8]> = TableDefinition::new("skill_vectors");
/// session id -> BranchRow (json): the sandbox branch backing a conversation
const BRANCHES: TableDefinition<&str, &[u8]> = TableDefinition::new("branches");
/// child session id -> SubagentRow (json): a session spawned by another agent.
///
/// Parentage lives in its own table rather than as a field on `SessionMeta`,
/// because `SessionMeta` is a WIT record shared with every guest and widening
/// it breaks them at instantiation. A row here is also the authority on whether
/// a session is a sub-agent at all, which is what the depth guard reads.
const SUBAGENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("subagents");

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
            txn.open_table(OWNERS)?;
            txn.open_table(LOGINS)?;
            txn.open_table(USER_SPEND)?;
            txn.open_table(REVISIONS)?;
            txn.open_table(SNAPSHOTS)?;
            txn.open_table(SKILL_VECTORS)?;
            txn.open_table(BRANCHES)?;
            txn.open_table(SUBAGENTS)?;
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

    pub fn create_session(
        &self,
        title: Option<String>,
        mode: &str,
        owner: &str,
    ) -> Result<SessionMeta> {
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
            txn.open_table(OWNERS)?.insert(meta.id.as_str(), owner)?;
        }
        txn.commit()?;
        Ok(meta)
    }

    pub fn owner_of(&self, id: &str) -> Result<Option<String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(OWNERS)?;
        Ok(table.get(id)?.map(|v| v.value().to_owned()))
    }

    pub fn owner_of_root(&self, id: &str) -> Result<Option<String>> {
        let mut root = id.to_owned();
        while let Some(row) = self.get_subagent(&root)? {
            root = row.parent_id;
        }
        self.owner_of(&root)
    }

    pub fn set_owner(&self, id: &str, owner: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            tx.open_table(OWNERS)?.insert(id, owner)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn owners_map(&self) -> Result<HashMap<String, String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(OWNERS)?;
        let mut out = HashMap::new();
        for row in table.iter()? {
            let (k, v) = row?;
            out.insert(k.value().to_owned(), v.value().to_owned());
        }
        Ok(out)
    }

    /// Conversations that still need an owner, and those whose owner is only a
    /// placeholder.
    ///
    /// `stale` is the owner id to treat as not-really-owned — the `local` mode
    /// sentinel. It matters because ownership is stamped on *every* boot: a
    /// system that ran in local mode has already given all its conversations to
    /// `local`, so by the time real users exist there is nothing left that is
    /// unowned, `claim_unowned` claims nothing, and every conversation the user
    /// had is answered with "conversation belongs to another user" — by an
    /// owner that is not a user and can never log in. Pass `None` to ask only
    /// for the genuinely unowned.
    ///
    /// An owner that is the empty string counts as absent: nobody's id is "",
    /// and a session carrying one is as unreachable as an unowned one.
    ///
    /// Sub-agent sessions are excluded: a child belongs to its parent, and
    /// `owner_of_root` resolves it there.
    pub fn sessions_needing_an_owner(&self, stale: Option<&str>) -> Result<Vec<String>> {
        let tx = self.db.begin_read()?;
        let sessions = tx.open_table(SESSIONS)?;
        let children = tx.open_table(SUBAGENTS)?;
        let owners = tx.open_table(OWNERS)?;
        let mut out = Vec::new();
        for row in sessions.iter()? {
            let (id, _) = row?;
            if children.get(id.value())?.is_some() {
                continue;
            }
            let owned_by = owners.get(id.value())?;
            let claimable = match owned_by.as_ref().map(|v| v.value()) {
                // No row at all: written before ownership existed.
                None => true,
                // An empty owner is not a user either. It is what a session
                // created without one carries, and it strands a conversation
                // exactly as the sentinel does — nobody's id is "".
                Some("") => true,
                Some(current) => stale == Some(current),
            };
            if claimable {
                out.push(id.value().to_owned());
            }
        }
        Ok(out)
    }

    pub fn list_sessions_owned(
        &self,
        owner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionMeta>> {
        let tx = self.db.begin_read()?;
        let sessions = tx.open_table(SESSIONS)?;
        let children = tx.open_table(SUBAGENTS)?;
        let owners = tx.open_table(OWNERS)?;
        let mut out = Vec::new();
        for row in sessions.iter()? {
            let (id, v) = row?;
            if children.get(id.value())?.is_some() {
                continue;
            }
            if let Some(want) = owner {
                if owners.get(id.value())?.as_ref().map(|v| v.value()) != Some(want) {
                    continue;
                }
            }
            let meta: SessionMeta = serde_json::from_slice(v.value())?;
            if include_archived || !meta.archived {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    pub fn put_login(&self, hash: &str, row: &LoginRow) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            tx.open_table(LOGINS)?
                .insert(hash, serde_json::to_vec(row)?.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn get_login(&self, hash: &str) -> Result<Option<LoginRow>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LOGINS)?;
        Ok(t.get(hash)?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?)
    }
    pub fn remove_login(&self, hash: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            tx.open_table(LOGINS)?.remove(hash)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn touch_login(&self, hash: &str, now: u64, expires: u64) -> Result<()> {
        if let Some(mut r) = self.get_login(hash)? {
            r.last_seen_ms = now;
            r.expires_ms = expires;
            self.put_login(hash, &r)?;
        }
        Ok(())
    }
    pub fn remove_logins_for(&self, user: &str) -> Result<usize> {
        let tx = self.db.begin_write()?;
        let mut n = 0;
        {
            let mut t = tx.open_table(LOGINS)?;
            let mut keys = Vec::new();
            for row in t.iter()? {
                let (k, v) = row?;
                let r: LoginRow = serde_json::from_slice(v.value())?;
                if r.user_id == user {
                    keys.push(k.value().to_owned());
                }
            }
            for k in keys {
                t.remove(k.as_str())?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }
    pub fn prune_expired_logins(&self, now: u64) -> Result<usize> {
        let tx = self.db.begin_write()?;
        let mut n = 0;
        {
            let mut t = tx.open_table(LOGINS)?;
            let mut keys = Vec::new();
            for row in t.iter()? {
                let (k, v) = row?;
                let r: LoginRow = serde_json::from_slice(v.value())?;
                if r.expires_ms <= now {
                    keys.push(k.value().to_owned());
                }
            }
            for k in keys {
                t.remove(k.as_str())?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }
    pub fn get_user_spend(&self, user: &str) -> Result<f64> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(USER_SPEND)?;
        Ok(t.get(user)?.map(|v| v.value()).unwrap_or(0.0))
    }
    pub fn add_user_spend(&self, user: &str, usd: f64) -> Result<f64> {
        let tx = self.db.begin_write()?;
        let total;
        {
            let mut table = tx.open_table(USER_SPEND)?;
            total = table.get(user)?.map(|v| v.value()).unwrap_or(0.0) + usd;
            table.insert(user, total)?;
        }
        tx.commit()?;
        Ok(total)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionMeta>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SESSIONS)?;
        match t.get(id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    /// Top-level conversations, most recently active first.
    ///
    /// A sub-agent has a session of its own but is not a conversation: it
    /// belongs to the turn that spawned it and is shown nested inside its
    /// parent's transcript. Listing it beside real conversations would fill the
    /// sidebar with rows nobody started, so children are filtered out here —
    /// one place, so every surface that lists sessions agrees.
    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionMeta>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SESSIONS)?;
        let children = txn.open_table(SUBAGENTS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (id, v) = row?;
            if children.get(id.value())?.is_some() {
                continue;
            }
            let meta: SessionMeta = serde_json::from_slice(v.value())?;
            if include_archived || !meta.archived {
                out.push(meta);
            }
        }
        // Most recently active first — the order the sidebar wants.
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    /// Every session, with its sub-agent registry row when it has one.
    ///
    /// The counterpart to [`Self::list_sessions`], which drops children because
    /// the sidebar wants conversations. Transcript search wants the opposite
    /// default available to it: a sub-agent's log is often exactly what someone
    /// is looking for, and it is the caller's business whether to include it.
    ///
    /// Both tables are read in **one** transaction, so a sub-agent registered
    /// while this runs cannot be seen as a top-level conversation. Doing it as
    /// two calls was the obvious shape and had that race in it.
    pub fn sessions_with_subagent_rows(
        &self,
        include_archived: bool,
    ) -> Result<Vec<(SessionMeta, Option<SubagentRow>)>> {
        let txn = self.db.begin_read()?;
        let sessions = txn.open_table(SESSIONS)?;
        let children = txn.open_table(SUBAGENTS)?;
        let mut out = Vec::new();
        for row in sessions.iter()? {
            let (id, v) = row?;
            let meta: SessionMeta = serde_json::from_slice(v.value())?;
            if !include_archived && meta.archived {
                continue;
            }
            let child = match children.get(id.value())? {
                Some(c) => Some(serde_json::from_slice::<SubagentRow>(c.value())?),
                None => None,
            };
            out.push((meta, child));
        }
        Ok(out)
    }

    // --- sub-agents ---------------------------------------------------------

    pub fn put_subagent(&self, row: &SubagentRow) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(SUBAGENTS)?;
            t.insert(row.child_id.as_str(), serde_json::to_vec(row)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_subagent(&self, child_id: &str) -> Result<Option<SubagentRow>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SUBAGENTS)?;
        match t.get(child_id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    /// Every sub-agent spawned by one session, oldest first.
    pub fn subagents_of(&self, parent_id: &str) -> Result<Vec<SubagentRow>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SUBAGENTS)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (_, v) = entry?;
            let row: SubagentRow = serde_json::from_slice(v.value())?;
            if row.parent_id == parent_id {
                out.push(row);
            }
        }
        out.sort_by_key(|r| r.created_ms);
        Ok(out)
    }

    /// What a session has done *while it is still doing it*.
    ///
    /// The sub-agent registry only learns what a child cost when the child
    /// settles, so a parent watching seven busy children saw seven identical
    /// `$0.0000` lines however hard they were working — and, having no way to
    /// tell busy from stuck, eventually cancelled all seven. These are the two
    /// places that *do* move during a turn: the spend ledger, written after
    /// every completion, and the session's own metadata, restamped on every
    /// appended event.
    pub fn session_progress(&self, session_id: &str) -> Result<SessionProgress> {
        let meta = self.get_session(session_id)?;
        Ok(SessionProgress {
            cost_usd: self.get_spend(session_id)?,
            events: meta.as_ref().map(|m| m.event_count).unwrap_or(0),
            activity_ms: meta.map(|m| m.updated_ms).unwrap_or(0),
        })
    }

    /// Every sub-agent row in the database, whoever owns it.
    ///
    /// Only the startup sweep wants this: everything else asks by parent or by
    /// root, because everything else is acting for one conversation.
    pub fn all_subagents(&self) -> Result<Vec<SubagentRow>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SUBAGENTS)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (_, v) = entry?;
            out.push(serde_json::from_slice(v.value())?);
        }
        out.sort_by_key(|r: &SubagentRow| r.created_ms);
        Ok(out)
    }

    /// Every sub-agent under one root conversation, whatever its depth.
    pub fn subagents_under(&self, root_id: &str) -> Result<Vec<SubagentRow>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(SUBAGENTS)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (_, v) = entry?;
            let row: SubagentRow = serde_json::from_slice(v.value())?;
            if row.root_id == root_id {
                out.push(row);
            }
        }
        out.sort_by_key(|r| r.created_ms);
        Ok(out)
    }

    /// Applies `f` to a sub-agent row in one transaction.
    pub fn update_subagent<F>(&self, child_id: &str, f: F) -> Result<SubagentRow>
    where
        F: FnOnce(&mut SubagentRow),
    {
        let txn = self.db.begin_write()?;
        let row = {
            let mut t = txn.open_table(SUBAGENTS)?;
            let mut row: SubagentRow = match t.get(child_id)? {
                Some(v) => serde_json::from_slice(v.value())?,
                None => return Err(anyhow!("no such sub-agent: {child_id}")),
            };
            f(&mut row);
            t.insert(child_id, serde_json::to_vec(&row)?.as_slice())?;
            row
        };
        txn.commit()?;
        Ok(row)
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

            let record = EventRecord {
                seq,
                ts_ms: ts,
                event,
            };
            let mut events = txn.open_table(EVENTS)?;
            events.insert((session_id, seq), serde_json::to_vec(&record)?.as_slice())?;
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

            // Did we ask for this? Read — and consumed — before anything else
            // looks at the log, because it changes what the log *means*. See
            // the marker scan below, and the resume budget further down.
            //
            // Clearing a flag writes an empty value rather than removing the
            // row, so presence alone does not mean set.
            let expected = self
                .kv_get(&meta.id, EXPECTED_RESTART_KEY)?
                .is_some_and(|v| !v.is_empty());
            if expected {
                self.kv_put(&meta.id, EXPECTED_RESTART_KEY, "")?;
            }

            // Where the turn stands. Only `turn-started` and `turn-finished`
            // are boundaries; an incident counts as an ending because a turn
            // that dies saying why often never reaches `turn-finished`.
            //
            // Except when the interruption was ours. A kernel rebuild
            // announces itself in the log — "restarting onto it now" — and
            // *then* kills the worker mid-turn. Read as an ending, that
            // announcement made every such restart look like a turn that had
            // already stopped, so the session was skipped, its dangling
            // `turn-started` never repaired and its turn never resumed. Three
            // turns in one conversation sat dead that way until somebody
            // typed "Continue". With the flag set, our own announcement is
            // not evidence that anything ended.
            let last_marker = events.iter().rev().find(|r| match &r.event {
                SessionEvent::TurnStarted | SessionEvent::TurnFinished(_) => true,
                SessionEvent::Incident(_) => !expected,
                _ => false,
            });
            if !matches!(
                last_marker.map(|r| &r.event),
                Some(SessionEvent::TurnStarted)
            ) {
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

            let opted_out = self
                .kv_get(&meta.id, NO_RESUME_KEY)?
                .is_some_and(|v| !v.is_empty());
            let attempts = self
                .kv_get(&meta.id, RESUME_ATTEMPTS_KEY)?
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);

            // `expected` was read above. Adopting a rebuilt kernel means
            // stopping the worker mid-turn on purpose, and that is the
            // supported way to pick up your own changes — several agents do it
            // repeatedly in one turn. Counting it as a death spent the whole
            // budget on healthy work and abandoned the turn with "interrupted
            // 2 times".
            //
            // The guard itself dates from the single-process orchestrator,
            // where a turn that died really did take everything with it. With
            // a worker per conversation a dying turn costs only its own worker,
            // so the budget is only meant for a turn that crashes it *by
            // itself*, repeatedly.

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

    /// Writes `value` only if the key currently holds `expected`. Reports
    /// whether it did.
    ///
    /// The read and the write share one write transaction, and redb serializes
    /// those, so two callers racing on the same key cannot both win. That is
    /// what lets a caller own a state transition rather than merely perform
    /// one: a read-modify-write through [`Self::kv_get`] and [`Self::kv_put`]
    /// lets both racers load the same state and both save a successor, and the
    /// loser's write silently wins.
    ///
    /// An absent key and an empty value are the same `""`, because there is no
    /// delete: clearing a key writes empty, and a caller that wants "create
    /// only if nothing is there" must be able to say so.
    pub fn kv_swap(&self, scope: &str, key: &str, expected: &str, value: &str) -> Result<bool> {
        let txn = self.db.begin_write()?;
        let swapped = {
            let mut t = txn.open_table(KV)?;
            let current = t.get((scope, key))?.map(|v| v.value().to_string());
            if current.unwrap_or_default() == expected {
                t.insert((scope, key), value)?;
                true
            } else {
                false
            }
        };
        // Committed either way: an abort would be equivalent here, but a commit
        // keeps the failure path from depending on redb's rollback behaviour.
        txn.commit()?;
        Ok(swapped)
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

    pub fn put_revision<T: serde::Serialize>(
        &self,
        aspect_key: &str,
        revision: u64,
        row: &T,
    ) -> Result<()> {
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

    pub fn list_revisions<T: serde::de::DeserializeOwned>(
        &self,
        aspect_key: &str,
    ) -> Result<Vec<T>> {
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

    // The trap this exists to close. Local mode stamps `local` on everything,
    // so by the time real users are configured nothing is unowned — and a
    // claim that only looks for unowned conversations moves none of them.
    #[test]
    fn switching_to_users_adopts_the_conversations_local_mode_claimed() {
        let (store, _d) = temp_store();
        let a = store.create_session(None, "agent", "").unwrap();
        let b = store.create_session(None, "agent", "").unwrap();

        // A local-mode boot: everything unowned becomes `local`.
        let first = store.sessions_needing_an_owner(None).unwrap();
        assert_eq!(first.len(), 2, "both start out unowned");
        for id in &first {
            store.set_owner(id, crate::auth::LOCAL_OWNER).unwrap();
        }
        // A second local-mode boot has nothing left to do.
        assert!(store.sessions_needing_an_owner(None).unwrap().is_empty());

        // Now users mode. Without treating the sentinel as unclaimed, this is
        // empty and the user's whole history is stranded.
        let next = store
            .sessions_needing_an_owner(Some(crate::auth::LOCAL_OWNER))
            .unwrap();
        assert_eq!(next.len(), 2, "the placeholder must not look like an owner");
        for id in &next {
            store.set_owner(id, "alice").unwrap();
        }

        let hers = store.list_sessions_owned(Some("alice"), false).unwrap();
        assert_eq!(hers.len(), 2, "she can see what she had");
        assert_eq!(store.owner_of(&a.id).unwrap().as_deref(), Some("alice"));
        assert_eq!(store.owner_of(&b.id).unwrap().as_deref(), Some("alice"));
    }

    // Claiming must never take a conversation off a real user, whatever the
    // sentinel says.
    #[test]
    fn a_conversation_owned_by_a_real_user_is_never_reclaimed() {
        let (store, _d) = temp_store();
        store.create_session(None, "agent", "bob").unwrap();
        store.create_session(None, "agent", "").unwrap();

        let claimable = store
            .sessions_needing_an_owner(Some(crate::auth::LOCAL_OWNER))
            .unwrap();
        assert_eq!(claimable.len(), 1, "only the unowned one: {claimable:?}");
        let owners = store.owners_map().unwrap();
        assert!(owners.values().any(|o| o == "bob"));
    }

    // A sub-agent is not a conversation; its owner is resolved through its
    // parent, and stamping one directly would be a second source of truth.
    #[test]
    fn sub_agents_are_not_claimed_in_their_own_right() {
        let (store, _d) = temp_store();
        let parent = store.create_session(None, "agent", "").unwrap();
        let child = store.create_session(None, "agent", "").unwrap();
        let subs = crate::subagents::Subagents::new(&store);
        subs.register(&parent.id, &child.id, "c", "t", "", "", "agent", 8)
            .unwrap();

        let claimable = store.sessions_needing_an_owner(None).unwrap();
        assert_eq!(claimable, vec![parent.id.clone()], "{claimable:?}");
    }

    #[test]
    fn ownership_logins_and_user_spend_round_trip() {
        let (store, _d) = temp_store();
        let alice = store
            .create_session(Some("Alice".into()), "agent", "alice")
            .unwrap();
        let bob = store
            .create_session(Some("Bob".into()), "agent", "bob")
            .unwrap();
        assert_eq!(store.owner_of(&alice.id).unwrap().as_deref(), Some("alice"));
        assert_eq!(
            store
                .list_sessions_owned(Some("alice"), false)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store.list_sessions_owned(Some("bob"), false).unwrap()[0].id,
            bob.id
        );

        let row = LoginRow {
            user_id: "alice".into(),
            created_ms: 10,
            last_seen_ms: 10,
            expires_ms: 20,
            user_agent: "test".into(),
        };
        store.put_login("token", &row).unwrap();
        assert_eq!(store.get_login("token").unwrap(), Some(row.clone()));
        store.touch_login("token", 15, 30).unwrap();
        assert_eq!(store.get_login("token").unwrap().unwrap().expires_ms, 30);
        assert_eq!(store.prune_expired_logins(31).unwrap(), 1);
        assert!(store.get_login("token").unwrap().is_none());

        assert_eq!(store.add_user_spend("alice", 1.25).unwrap(), 1.25);
        assert_eq!(store.add_user_spend("alice", 0.75).unwrap(), 2.0);
        assert_eq!(store.get_user_spend("alice").unwrap(), 2.0);
    }

    #[test]
    fn archiving_only_sets_a_flag_and_get_session_still_returns_it() {
        // The property the Discord connector depends on: archiving is not a
        // delete, so checking that a mapped session merely *exists* would let a
        // chat surface carry on in an archived conversation. Anything reusing a
        // session id has to read `archived`.
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"chat", "local").unwrap();
        assert!(!s.archived);

        store.archive_session(&s.id, true).unwrap();

        let found = store.get_session(&s.id).unwrap().expect("still present");
        assert!(found.archived, "archiving must set the flag");
        assert!(
            !store
                .list_sessions(false)
                .unwrap()
                .iter()
                .any(|m| m.id == s.id),
            "an archived session is hidden from the default listing"
        );
        assert!(
            store
                .list_sessions(true)
                .unwrap()
                .iter()
                .any(|m| m.id == s.id),
            "and still available when archived ones are asked for"
        );
    }

    #[test]
    fn events_are_sequential_and_readable_from_offset() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();

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
        let a = store
            .create_session(Some("a".into()), &"agent", "local")
            .unwrap();
        let b = store
            .create_session(Some("b".into()), &"agent", "local")
            .unwrap();

        store.append_event(&a.id, user("in a")).unwrap();
        store.append_event(&b.id, user("in b")).unwrap();

        assert_eq!(store.events(&a.id, 0).unwrap().len(), 1);
        assert_eq!(store.events(&b.id, 0).unwrap().len(), 1);
    }

    #[test]
    fn preview_tracks_conversation_not_bookkeeping() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();

        store.append_event(&s.id, user("hello there")).unwrap();
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
        let s = store.create_session(None, &"agent", "local").unwrap();
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
        let s = store.create_session(None, &"agent", "local").unwrap();

        store
            .append_event(
                &s.id,
                user("please explain in detail how the revision registry decides what counts as a known good build"),
            )
            .unwrap();

        let title = store.get_session(&s.id).unwrap().unwrap().title;
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= 49, "{title}");
        assert!(
            !title.contains("  "),
            "newlines and runs of spaces collapse"
        );
        // The cut lands between words, not inside one.
        assert!(
            title.starts_with("please explain in detail how the revision"),
            "{title}"
        );
    }

    #[test]
    fn a_renamed_conversation_is_never_auto_titled() {
        let (store, _d) = temp_store();
        let s = store
            .create_session(Some("Budget review".into()), &"agent", "local")
            .unwrap();

        store.append_event(&s.id, user("hello there")).unwrap();

        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().title,
            "Budget review"
        );
    }

    #[test]
    fn image_only_conversations_are_named_after_the_file() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();

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

        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().title,
            "receipt.png"
        );
    }

    #[test]
    fn session_mode_and_model_round_trip() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();
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
        let s = store.create_session(None, &"agent", "local").unwrap();

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
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().preview,
            "[chart.png]"
        );

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
        let s = store
            .create_session(Some(title.into()), &"agent", "local")
            .unwrap();
        store.append_event(&s.id, user("do the thing")).unwrap();
        store
            .append_event(&s.id, SessionEvent::TurnStarted)
            .unwrap();

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

        let found = store
            .reconcile_interrupted_turns("restarted", &[], None)
            .unwrap();
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

        store
            .reconcile_interrupted_turns("restarted", &[], None)
            .unwrap();

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

        store
            .reconcile_interrupted_turns("restarted", &[], None)
            .unwrap();

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

        let found = store
            .reconcile_interrupted_turns("restarted", &[], None)
            .unwrap();
        assert_eq!(found.len(), 1);
        assert!(!found[0].resume);

        // The turn is closed rather than left looking like it is still running.
        assert!(matches!(
            store.events(&id, 0).unwrap().last().map(|r| &r.event),
            Some(SessionEvent::TurnFinished(_))
        ));

        // The preference is spent, so a later interruption resumes normally.
        store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        let again = store
            .reconcile_interrupted_turns("restarted", &[], None)
            .unwrap();
        assert!(again[0].resume);
    }

    #[test]
    fn a_turn_that_keeps_dying_stops_being_resumed() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "doomed", None);

        // Each pass ends mid-turn again, as it would if resuming crashed.
        for expected in [true, true, false] {
            let found = store
                .reconcile_interrupted_turns("restarted", &[], None)
                .unwrap();
            assert_eq!(found[0].resume, expected, "attempt outcome");
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        }

        assert!(
            store.events(&id, 0).unwrap().iter().any(
                |r| matches!(&r.event, SessionEvent::Incident(t) if t.contains("not resuming"))
            )
        );
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
            !store.events(&id, 0).unwrap().iter().any(
                |r| matches!(&r.event, SessionEvent::Incident(t) if t.contains("not resuming"))
            ),
            "a turn making progress must never be abandoned"
        );
    }

    // The exact log a kernel rebuild leaves behind, and the reason three turns
    // in one conversation sat dead until somebody typed "Continue":
    //
    //   turn-started ... tool-call(wait) ... incident("Restarting onto it now")
    //
    // The scan read that trailing incident as the turn's ending, skipped the
    // session, and so never repaired the dangling call or resumed the turn.
    #[test]
    fn our_own_restart_announcement_does_not_end_the_turn_it_interrupts() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "rebuilding its own kernel", Some("call-1"));

        // What `request_restart` records before the process goes away, and
        // what `build_then_restart` writes just before it does.
        store.expect_restart(&id).unwrap();
        store
            .append_event(
                &id,
                SessionEvent::Incident(
                    "The orchestrator rebuilt from your changes and passed its startup probe. \
                     Restarting onto it now."
                        .into(),
                ),
            )
            .unwrap();

        let found = store
            .reconcile_interrupted_turns("picked back up", &[], None)
            .unwrap();
        assert_eq!(found.len(), 1, "the turn was interrupted, not finished");
        assert!(found[0].resume, "and `resume: true` was what was asked for");

        // The tool call the restart cut off has to be answered, or the next
        // request carries a call with no result and the provider rejects it.
        let events = store.events(&id, 0).unwrap();
        assert!(
            events.iter().any(|r| matches!(
                &r.event,
                SessionEvent::ToolResult(o) if o.call_id == "call-1" && !o.ok
            )),
            "the interrupted call was never answered"
        );
    }

    // An incident is still an ending when it was not our doing — a turn that
    // died reporting why must not be resumed on the strength of this change.
    #[test]
    fn an_unexpected_incident_still_reads_as_the_turn_ending() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "a turn that broke", None);
        store
            .append_event(
                &id,
                SessionEvent::Incident("transport error: the model provider went away".into()),
            )
            .unwrap();

        let found = store
            .reconcile_interrupted_turns("picked back up", &[], None)
            .unwrap();
        assert!(
            found.is_empty(),
            "nothing asked for this interruption, so the incident ends the turn"
        );
    }

    // The marker is one-shot in both directions: spending it on a session that
    // turns out not to be interrupted must not leave it armed for a later
    // crash that nobody asked for.
    #[test]
    fn an_unused_restart_marker_is_still_consumed() {
        let (store, _d) = temp_store();
        let id = mid_turn(&store, "restarted between turns", None);
        // The turn ended cleanly before the restart landed.
        finish_turn(&store, &id);
        store.expect_restart(&id).unwrap();

        assert!(
            store
                .reconcile_interrupted_turns("picked back up", &[], None)
                .unwrap()
                .is_empty(),
            "a finished turn is not interrupted"
        );

        // Now a real crash, with an incident and no `turn-finished`. The spent
        // marker must not excuse it.
        store.append_event(&id, SessionEvent::TurnStarted).unwrap();
        store
            .append_event(&id, SessionEvent::Incident("it fell over".into()))
            .unwrap();
        assert!(
            store
                .reconcile_interrupted_turns("picked back up", &[], None)
                .unwrap()
                .is_empty(),
            "the marker was consumed by the sweep that did not need it"
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
            !store.events(&id, 0).unwrap().iter().any(
                |r| matches!(&r.event, SessionEvent::Incident(t) if t.contains("not resuming"))
            ),
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

        store
            .reconcile_interrupted_turns("restarted", &[], None)
            .unwrap();
        store.clear_resume_attempts(&id).unwrap();

        // Back to a clean slate: the cap starts counting from zero again.
        for _ in 0..2 {
            store.append_event(&id, SessionEvent::TurnStarted).unwrap();
            let found = store
                .reconcile_interrupted_turns("restarted", &[], None)
                .unwrap();
            assert!(found[0].resume);
        }
    }

    #[test]
    fn spend_accumulates() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();
        assert_eq!(store.get_spend(&s.id).unwrap(), 0.0);
        store.add_spend(&s.id, 0.25).unwrap();
        let total = store.add_spend(&s.id, 0.5).unwrap();
        assert!((total - 0.75).abs() < f64::EPSILON);
    }

    // The numbers a parent needs while a child is still working. Reading them
    // from the row instead gives 0.0 and nothing else until the child settles,
    // which is what made a working fan-out look like a hung one.
    #[test]
    fn progress_is_readable_before_a_session_has_finished_anything() {
        let (store, _d) = temp_store();
        let s = store.create_session(None, &"agent", "local").unwrap();

        let fresh = store.session_progress(&s.id).unwrap();
        assert_eq!(fresh.cost_usd, 0.0);
        assert_eq!(fresh.events, 0);

        store.add_spend(&s.id, 1.25).unwrap();
        store
            .append_event(&s.id, SessionEvent::TurnStarted)
            .unwrap();
        store
            .append_event(&s.id, SessionEvent::SystemNote("working".into()))
            .unwrap();

        let moving = store.session_progress(&s.id).unwrap();
        assert!((moving.cost_usd - 1.25).abs() < f64::EPSILON);
        assert_eq!(moving.events, 2);
        assert!(moving.activity_ms > 0, "the clock has to move too");
    }

    #[test]
    fn progress_for_a_session_that_does_not_exist_is_zero_not_an_error() {
        let (store, _d) = temp_store();
        let p = store.session_progress("never-created").unwrap();
        assert_eq!(p, SessionProgress::default());
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
    fn kv_swap_writes_only_when_the_current_value_matches() {
        let (store, _d) = temp_store();
        // An absent key reads as empty, which is how "create if nothing is
        // there" is expressed — there is no delete, so empty and absent are one.
        assert!(store.kv_swap("global", "k", "", "first").unwrap());
        assert_eq!(
            store.kv_get("global", "k").unwrap().as_deref(),
            Some("first")
        );

        // The same claim a second time loses: this is what stops two readers of
        // one ask_user call from both posting a form.
        assert!(!store.kv_swap("global", "k", "", "second").unwrap());
        assert_eq!(
            store.kv_get("global", "k").unwrap().as_deref(),
            Some("first")
        );

        // A transition from the value actually held succeeds.
        assert!(store.kv_swap("global", "k", "first", "next").unwrap());
        assert_eq!(
            store.kv_get("global", "k").unwrap().as_deref(),
            Some("next")
        );
        // And the same transition replayed does not, so a duplicated click
        // cannot apply an answer twice.
        assert!(!store.kv_swap("global", "k", "first", "next").unwrap());
    }

    #[test]
    fn kv_swap_can_clear_a_key_and_only_the_first_caller_wins() {
        let (store, _d) = temp_store();
        store.kv_put("global", "form", "state").unwrap();
        // Clearing through the swap is what makes "this click finished the form"
        // observable by exactly one caller, and therefore what stops two
        // callers from both submitting the answers.
        assert!(store.kv_swap("global", "form", "state", "").unwrap());
        assert!(!store.kv_swap("global", "form", "state", "").unwrap());
        assert_eq!(store.kv_get("global", "form").unwrap().as_deref(), Some(""));
    }

    #[test]
    fn kv_swap_is_scoped_like_the_rest_of_the_table() {
        let (store, _d) = temp_store();
        assert!(store.kv_swap("global", "k", "", "g").unwrap());
        // A different scope is a different key, so claiming in one does not
        // block the other.
        assert!(store.kv_swap("sess-1", "k", "", "s").unwrap());
        assert_eq!(store.kv_get("global", "k").unwrap().as_deref(), Some("g"));
        assert_eq!(store.kv_get("sess-1", "k").unwrap().as_deref(), Some("s"));
    }

    /// The property the fix rests on: under real concurrency exactly one
    /// claimant wins. A read-then-write through `kv_get`/`kv_put` would let
    /// several threads all see empty and all write, which is the bug.
    #[test]
    fn concurrent_claims_on_one_key_produce_exactly_one_winner() {
        let (store, _d) = temp_store();
        let store = std::sync::Arc::new(store);
        let wins = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let threads: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                let wins = wins.clone();
                std::thread::spawn(move || {
                    if store
                        .kv_swap("global", "call", "", &format!("form-{i}"))
                        .unwrap()
                    {
                        wins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(
            wins.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one claimant may win, or one question gets two forms"
        );
    }

    #[test]
    fn archived_sessions_are_filtered() {
        let (store, _d) = temp_store();
        let s = store
            .create_session(Some("keep".into()), &"agent", "local")
            .unwrap();
        let g = store
            .create_session(Some("gone".into()), &"agent", "local")
            .unwrap();
        store.archive_session(&g.id, true).unwrap();

        let visible = store.list_sessions(false).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, s.id);
        assert_eq!(store.list_sessions(true).unwrap().len(), 2);
    }
}
