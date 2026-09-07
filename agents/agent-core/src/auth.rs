//! Native authentication and ownership checks.
use crate::{config::Config, grip::Grip, policy::EffectivePolicy};
use anyhow::{Context, Result, bail};
use argon2::password_hash::{SaltString, rand_core::OsRng};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::http::{HeaderMap, header};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const COOKIE: &str = "thetis_session";
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub policy: Arc<EffectivePolicy>,
    pub view_all: Arc<AtomicBool>,
}
pub const LOCAL_OWNER: &str = "local";

impl Principal {
    pub fn new(user_id: String, display_name: String, role: String, policy: Arc<EffectivePolicy>) -> Self {
        let view_all = policy.admin;
        Self { user_id, display_name, role, policy, view_all: Arc::new(AtomicBool::new(view_all)) }
    }
    pub fn is_admin(&self) -> bool { self.policy.admin }
    pub fn local(c: &Config) -> Arc<Self> {
        Arc::new(Self::new(LOCAL_OWNER.into(), LOCAL_OWNER.into(), "admin".into(), c.auth.local_policy.clone()))
    }
    pub fn from_user(u: &crate::config::UserSpec) -> Arc<Self> {
        Arc::new(Self::new(u.id.clone(), u.name.clone(), u.role.clone(), u.policy.clone()))
    }
    pub fn may_see_all(&self) -> bool { self.is_admin() || self.policy.see_all_sessions }
    pub fn viewing_all(&self) -> bool { self.may_see_all() && self.view_all.load(Ordering::Relaxed) }
    pub fn set_view_all(&self, on: bool) { self.view_all.store(on, Ordering::Relaxed); }
    pub fn list_owner(&self) -> Option<&str> {
        if self.viewing_all() { None } else { Some(self.user_id.as_str()) }
    }
    pub fn describe(&self) -> serde_json::Value {
        use crate::policy::Cap;
        let denied: Vec<Cap> = Cap::all().iter().copied().filter(|cap| self.policy.denies(*cap)).collect();
        serde_json::json!({
            "id": self.user_id, "name": self.display_name, "role": self.role,
            "admin": self.is_admin(), "read_only": self.policy.read_only,
            "see_all": self.may_see_all(), "viewing_all": self.viewing_all(),
            "workspace": if self.policy.denies(Cap::Workspace) { "none" } else if self.policy.denies(Cap::WorkspaceWrite) { "read" } else { "write" },
            "denied": denied, "models_restricted": self.policy.models_restricted,
            "local": !self.is_account(),
        })
    }
    pub fn is_account(&self) -> bool {
        self.user_id != LOCAL_OWNER || self.role != "admin" || self.display_name != LOCAL_OWNER
    }
}
