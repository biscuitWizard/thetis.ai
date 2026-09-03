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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub const COOKIE: &str = "thetis_session";
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub policy: Arc<EffectivePolicy>,
}
/// The owner every conversation gets in `auth.mode = "local"`.
///
/// A placeholder rather than an account: nobody can log in as it, and the
/// startup claim in `roles::gateway` re-owns anything wearing it as soon as
/// real users exist. Naming it matters because ownership rows written under it
/// have to be recognised later, and a bare `"local"` spread across three files
/// is not something you can recognise. (A configured user whose id happens to
/// be `local` simply inherits its conversations, which is the right answer.)
pub const LOCAL_OWNER: &str = "local";

impl Principal {
    pub fn is_admin(&self) -> bool {
        self.policy.admin
    }
    pub fn local(c: &Config) -> Arc<Self> {
        Arc::new(Self {
            user_id: LOCAL_OWNER.into(),
            display_name: LOCAL_OWNER.into(),
            role: "admin".into(),
            policy: c.auth.local_policy.clone(),
        })
    }
    pub fn from_user(u: &crate::config::UserSpec) -> Arc<Self> {
        Arc::new(Self {
            user_id: u.id.clone(),
            display_name: u.name.clone(),
            role: u.role.clone(),
            policy: u.policy.clone(),
        })
    }
}
pub fn hash_password(p: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(p.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing: {e}"))?
        .to_string())
}
pub fn verify_password(p: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .ok()
        .is_some_and(|h| Argon2::default().verify_password(p.as_bytes(), &h).is_ok())
}
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}
pub fn token_hash(t: &str) -> String {
    hex::encode(Sha256::digest(t.as_bytes()))
}
pub fn cookie_value(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            (k == COOKIE).then(|| v.into())
        })
}
pub fn set_cookie(c: &Config, t: &str) -> String {
    format!(
        "{COOKIE}={t}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        c.auth.session_ttl.as_secs(),
        if c.public_origin
            .as_ref()
            .is_some_and(|o| o.scheme == "https")
        {
            "; Secure"
        } else {
            ""
        }
    )
}
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}
pub fn safe_next(s: &str) -> String {
    if s.starts_with('/') && !s.starts_with("//") && !s.contains("://") {
        s.into()
    } else {
        "/".into()
    }
}

static FAILURES: OnceLock<Mutex<HashMap<String, (u32, Instant)>>> = OnceLock::new();
fn failures() -> &'static Mutex<HashMap<String, (u32, Instant)>> {
    FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}
pub fn login_locked(user: &str, c: &Config) -> bool {
    let mut rows = failures().lock().expect("login lockout mutex");
    let Some((count, since)) = rows.get(user).copied() else {
        return false;
    };
    if since.elapsed() >= c.auth.lockout {
        rows.remove(user);
        return false;
    }
    count >= c.auth.lockout_after
}
pub fn login_failed(user: &str, c: &Config) {
    let mut rows = failures().lock().expect("login lockout mutex");
    let row = rows.entry(user.to_string()).or_insert((0, Instant::now()));
    if row.1.elapsed() >= c.auth.lockout {
        *row = (0, Instant::now());
    }
    row.0 = row.0.saturating_add(1);
}
pub fn login_succeeded(user: &str) {
    failures().lock().expect("login lockout mutex").remove(user);
}
pub async fn resolve(g: &Arc<Grip>, h: &HeaderMap) -> Option<Arc<Principal>> {
    if !g.cfg.auth.users_mode {
        return Some(Principal::local(&g.cfg));
    }
    let hash = token_hash(&cookie_value(h)?);
    let st = g.local_store()?;
    let row = st.get_login(&hash).ok().flatten()?;
    let now = crate::store::now_ms();
    if row.expires_ms <= now {
        let _ = st.remove_login(&hash);
        return None;
    }
    let Some(u) = g.cfg.auth.user(&row.user_id) else {
        let _ = st.remove_login(&hash);
        return None;
    };
    if now.saturating_sub(row.last_seen_ms) > 60_000 {
        let _ = st.touch_login(&hash, now, now + g.cfg.auth.session_ttl.as_millis() as u64);
    }
    Some(Principal::from_user(u))
}
pub fn may_access(g: &Grip, p: &Principal, id: &str) -> Result<()> {
    if p.policy.see_all_sessions {
        return Ok(());
    }
    let st = g.local_store().context("ownership is gateway-only")?;
    match st.owner_of_root(id)? {
        Some(o) if o == p.user_id => Ok(()),
        Some(_) => bail!("conversation belongs to another user"),
        None => bail!("no such conversation"),
    }
}
pub fn page(c: &Config, msg: Option<&str>, next: &str) -> axum::response::Html<String> {
    axum::response::Html(format!(
        "<!doctype html><meta charset=utf-8><title>{0} — sign in</title><style>body{{font:15px system-ui;max-width:24rem;margin:6rem auto;background:#16161a;color:#eee}}input,button{{font:inherit;padding:.6rem;margin:.3rem;width:100%;box-sizing:border-box}}</style><h1>{0}</h1><p>{1}</p><form method=post action=/login><input type=hidden name=next value=\"{2}\"><input name=user autocomplete=username required autofocus><input name=password type=password autocomplete=current-password required><button>Sign in</button></form>",
        c.agent_name,
        msg.unwrap_or(""),
        safe_next(next)
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashes() {
        let h = hash_password("x").unwrap();
        assert!(verify_password("x", &h));
        assert!(!verify_password("y", &h));
    }
    #[test]
    fn redirects() {
        assert_eq!(safe_next("https://bad"), "/");
        assert_eq!(safe_next("/ok"), "/ok");
    }
}
