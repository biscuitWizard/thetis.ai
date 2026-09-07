//! The legacy revision registry, read-only.
//!
//! Versioning now lives on each conversation's git branch, with built
//! artifacts in the content-addressed build cache. What remains here is just
//! enough to *read* what the old system recorded — so a deployment's first
//! boot after the migration can keep serving its UI from the last activated
//! artifact until a fresh build lands in the cache. The old
//! `artifacts/<aspect>/rNNNN` directories can be archived or deleted once that
//! has happened.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::aspect::Aspect;
use crate::config::Config;
use crate::persist::Persist;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Candidate,
    Active,
    KnownGood,
    RolledBack,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Bootstrap,
    HumanEdit,
    AgentMod,
    Rollback,
}

impl Origin {
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Bootstrap => "bootstrap",
            Origin::HumanEdit => "human-edit",
            Origin::AgentMod => "agent-mod",
            Origin::Rollback => "rollback",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionRow {
    pub aspect: String,
    pub revision: u64,
    pub status: Status,
    pub origin: Origin,
    pub note: String,
    pub created_ms: u64,
    pub hash: String,
}

pub struct Revisions {
    cfg: Arc<Config>,
    persist: Persist,
}

impl Revisions {
    pub fn new(cfg: Arc<Config>, persist: Persist) -> Self {
        Self { cfg, persist }
    }

    pub fn component_path(&self, aspect: &Aspect, revision: u64) -> PathBuf {
        self.cfg
            .aspect_artifact_dir(aspect, revision)
            .join("component.wasm")
    }

    pub async fn history(&self, aspect: &Aspect) -> Result<Vec<RevisionRow>> {
        self.persist
            .list_revisions(&aspect.key())
            .await?
            .into_iter()
            .map(|v| Ok(serde_json::from_value(v)?))
            .collect()
    }

    /// The revision the old system had activated, if any survived migration.
    pub async fn active(&self, aspect: &Aspect) -> Result<Option<RevisionRow>> {
        Ok(self
            .history(aspect)
            .await?
            .into_iter()
            .find(|r| r.status == Status::Active))
    }
}
