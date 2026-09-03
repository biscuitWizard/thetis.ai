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
    /// Whether the sidebar is currently showing everyone's conversations.
    ///
    /// Per connection, not per user: one `Principal` is resolved at the
    /// websocket upgrade and shared by every guest call made on that socket,
    /// so a flag here is a flag for that tab. It only means anything when the
    /// policy grants `see_all_sessions`; the sidebar stays personal by default
    /// even for an administrator, and the toggle in the UI flips this.
    pub view_all: Arc<AtomicBool>,
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
    pub fn new(user_id: String, display_name: String, role: String, policy: Arc<EffectivePolicy>) -> Self {
        Self {
            user_id,
            display_name,
            role,
            policy,
            view_all: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn is_admin(&self) -> bool {
        self.policy.admin
    }
    pub fn local(c: &Config) -> Arc<Self> {
        Arc::new(Self::new(
            LOCAL_OWNER.into(),
            LOCAL_OWNER.into(),
            "admin".into(),
            c.auth.local_policy.clone(),
        ))
    }
    pub fn from_user(u: &crate::config::UserSpec) -> Arc<Self> {
        Arc::new(Self::new(
            u.id.clone(),
            u.name.clone(),
            u.role.clone(),
            u.policy.clone(),
        ))
    }
    /// May this principal see everyone's conversations at all.
    pub fn may_see_all(&self) -> bool {
        self.policy.see_all_sessions
    }
    /// Is this connection currently asking for everyone's conversations.
    /// Never true for someone the policy does not allow it.
    pub fn viewing_all(&self) -> bool {
        self.may_see_all() && self.view_all.load(Ordering::Relaxed)
    }
    pub fn set_view_all(&self, on: bool) {
        self.view_all.store(on, Ordering::Relaxed);
    }
    /// Which owner a listing is filtered to: `None` means every conversation.
    pub fn list_owner(&self) -> Option<&str> {
        if self.viewing_all() {
            None
        } else {
            Some(self.user_id.as_str())
        }
    }
    /// The identity and policy summary the browser gets, both as the `user`
    /// frame on a fresh socket and from `GET /api/me`. One function so the
    /// two never disagree about a field name.
    pub fn describe(&self) -> serde_json::Value {
        use crate::policy::Cap;
        let denied: Vec<Cap> = Cap::all()
            .iter()
            .copied()
            .filter(|cap| self.policy.denies(*cap))
            .collect();
        serde_json::json!({
            "id": self.user_id,
            "name": self.display_name,
            "role": self.role,
            "admin": self.is_admin(),
            "read_only": self.policy.read_only,
            "see_all": self.may_see_all(),
            "viewing_all": self.viewing_all(),
            "workspace": if self.policy.denies(Cap::Workspace) {
                "none"
            } else if self.policy.denies(Cap::WorkspaceWrite) {
                "read"
            } else {
                "write"
            },
            "denied": denied,
            "models_restricted": self.policy.models_restricted,
            "local": !self.is_account(),
        })
    }
    /// A real account, as opposed to the implicit principal of local mode.
    pub fn is_account(&self) -> bool {
        self.user_id != LOCAL_OWNER || self.role != "admin" || self.display_name != LOCAL_OWNER
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
    lockout::is_locked(failures(), user, c.auth.lockout_after, c.auth.lockout)
}
pub fn login_failed(user: &str, c: &Config) {
    lockout::record_failure(failures(), user, c.auth.lockout)
}
pub fn login_succeeded(user: &str) {
    lockout::clear(failures(), user)
}

/// The cooling-off rule, over any table so a test can use its own.
mod lockout {
    use super::*;
    pub type Table = Mutex<HashMap<String, (u32, Instant)>>;

    pub fn is_locked(table: &Table, user: &str, after: u32, window: Duration) -> bool {
        let mut rows = table.lock().expect("login lockout mutex");
        let Some((count, since)) = rows.get(user).copied() else {
            return false;
        };
        if since.elapsed() >= window {
            rows.remove(user);
            return false;
        }
        // `after = 0` would lock every account before its first attempt.
        after > 0 && count >= after
    }
    pub fn record_failure(table: &Table, user: &str, window: Duration) {
        let mut rows = table.lock().expect("login lockout mutex");
        let row = rows.entry(user.to_string()).or_insert((0, Instant::now()));
        if row.1.elapsed() >= window {
            *row = (0, Instant::now());
        }
        row.0 = row.0.saturating_add(1);
    }
    pub fn clear(table: &Table, user: &str) {
        table.lock().expect("login lockout mutex").remove(user);
    }
}

/// A real hash of a throwaway password, made once, so rejecting a user that
/// does not exist costs the same argon2 work as rejecting a wrong password.
/// Otherwise the response time says which account names are configured.
pub fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("not-a-password").unwrap_or_default())
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
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The host-rendered sign-in page.
///
/// Deliberately plain: no script, no socket, no guest. It shares the app's
/// palette (see `ui/theme.css`) so arriving here does not feel like leaving
/// the product, and it follows the system's light/dark preference because a
/// login page is the one screen a person sees before any of their settings
/// have loaded.
pub fn page(c: &Config, msg: Option<&str>, next: &str) -> axum::response::Html<String> {
    axum::response::Html(page_html(&c.agent_name, msg, next))
}

fn page_html(agent_name: &str, msg: Option<&str>, next: &str) -> String {
    let banner = msg
        .map(|m| format!(r#"<p class="banner" role="alert">{}</p>"#, html_escape(m)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>{name} — sign in</title>
<style>
:root{{--bg:#0b0b0f;--surface:#101016;--surface-2:#16161f;--hairline:#24242f;--hairline-strong:#32323f;--text:#ececf2;--text-dim:#a3a3b4;--text-faint:#6e6e82;--accent:#7c9cff;--accent-hot:#96b0ff;--err:#f2788f;--err-wash:rgba(242,120,143,.12)}}
@media (prefers-color-scheme:light){{:root{{--bg:#f5f5f8;--surface:#ffffff;--surface-2:#ededf3;--hairline:#d9d9e3;--hairline-strong:#c3c3d1;--text:#17171f;--text-dim:#565669;--text-faint:#8a8a9c;--accent:#4a5fd0;--accent-hot:#3646b3;--err:#c0384f;--err-wash:rgba(192,56,79,.10)}}}}
*{{box-sizing:border-box}}
html,body{{height:100%}}
body{{margin:0;display:grid;place-items:center;padding:1.5rem;background:var(--bg);color:var(--text);font:15px/1.6 "Inter",ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}}
main{{width:100%;max-width:22rem;padding:1.75rem 1.75rem 1.5rem;background:var(--surface);border:1px solid var(--hairline);border-radius:14px;box-shadow:0 12px 40px rgba(0,0,0,.25)}}
.brand{{display:flex;align-items:center;gap:.6rem;margin:0 0 1.25rem}}
.brand svg{{width:22px;height:22px;color:var(--accent);flex:none}}
h1{{font-size:1.05rem;font-weight:600;margin:0;letter-spacing:.01em;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
p.lead{{margin:0 0 1rem;color:var(--text-dim);font-size:.9rem}}
label{{display:block;margin:.9rem 0 .3rem;color:var(--text-dim);font-size:.72rem;font-weight:600;text-transform:uppercase;letter-spacing:.08em}}
input{{font:inherit;width:100%;padding:.55rem .7rem;border-radius:8px;border:1px solid var(--hairline-strong);background:var(--surface-2);color:var(--text);outline:none;transition:border-color .12s,box-shadow .12s}}
input:focus{{border-color:var(--accent);box-shadow:0 0 0 3px color-mix(in srgb,var(--accent) 25%,transparent)}}
button{{font:inherit;font-weight:600;width:100%;margin-top:1.3rem;padding:.6rem .9rem;border-radius:8px;border:1px solid transparent;background:var(--accent);color:#fff;cursor:pointer;transition:background .12s}}
button:hover{{background:var(--accent-hot)}}
button:focus-visible{{outline:2px solid var(--accent-hot);outline-offset:2px}}
.banner{{margin:0 0 .5rem;padding:.55rem .75rem;border-radius:8px;font-size:.88rem;color:var(--err);background:var(--err-wash);border:1px solid color-mix(in srgb,var(--err) 40%,transparent)}}
.foot{{margin:1.25rem 0 0;color:var(--text-faint);font-size:.78rem;text-align:center}}
</style>
<main>
  <div class="brand">
    <svg viewBox="0 0 32 32" aria-hidden="true"><circle cx="16" cy="16" r="9" fill="none" stroke="currentColor" stroke-width="2.5"/><circle cx="16" cy="16" r="3" fill="currentColor"/></svg>
    <h1 title="{name}">{name}</h1>
  </div>
  <p class="lead">Sign in to your conversations.</p>
  {banner}
  <form method="post" action="/login">
    <input type="hidden" name="next" value="{next}">
    <label for="user">User</label>
    <input id="user" name="user" autocomplete="username" autocapitalize="none" spellcheck="false" required autofocus>
    <label for="password">Password</label>
    <input id="password" name="password" type="password" autocomplete="current-password" required>
    <button type="submit">Sign in</button>
  </form>
  <p class="foot">Accounts are configured by the operator of this Thetis.</p>
</main>
"#,
        name = html_escape(agent_name),
        banner = banner,
        next = html_escape(&safe_next(next))
    )
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
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("/x?u=http://a"), "/");
        assert_eq!(safe_next(""), "/");
        assert_eq!(safe_next("/ok"), "/ok");
        assert_eq!(safe_next("/ok?next=1#frag"), "/ok?next=1#frag");
    }

    #[test]
    fn the_cookie_is_found_among_others_and_across_headers() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, "theme=dark; thetis_session=abc123; other=1".parse().unwrap());
        assert_eq!(cookie_value(&h).as_deref(), Some("abc123"));

        let mut h = HeaderMap::new();
        h.append(header::COOKIE, "theme=dark".parse().unwrap());
        h.append(header::COOKIE, " thetis_session=xyz ".parse().unwrap());
        assert_eq!(cookie_value(&h).as_deref(), Some("xyz"));

        let mut h = HeaderMap::new();
        h.append(header::COOKIE, "thetis_session_old=nope; x=thetis_session=1".parse().unwrap());
        assert_eq!(cookie_value(&h), None);
    }

    #[test]
    fn tokens_are_unique_urlsafe_and_hashed_at_rest() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert!(a.len() >= 40, "{a}");
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_eq!(token_hash(&a).len(), 64);
        assert_ne!(token_hash(&a), token_hash(&b));
    }

    #[test]
    fn the_clear_cookie_expires_at_once() {
        let c = clear_cookie();
        assert!(c.starts_with("thetis_session=;"));
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("HttpOnly"));
    }

    #[test]
    fn lockout_counts_failures_within_the_window_and_forgets_after_it() {
        let table = lockout::Table::default();
        let window = Duration::from_secs(60);
        assert!(!lockout::is_locked(&table, "bob", 3, window));
        lockout::record_failure(&table, "bob", window);
        lockout::record_failure(&table, "bob", window);
        assert!(!lockout::is_locked(&table, "bob", 3, window), "two of three");
        lockout::record_failure(&table, "bob", window);
        assert!(lockout::is_locked(&table, "bob", 3, window));
        assert!(!lockout::is_locked(&table, "alice", 3, window), "per user");
        lockout::clear(&table, "bob");
        assert!(!lockout::is_locked(&table, "bob", 3, window), "success clears");

        // An expired window is forgotten, not carried forward.
        let short = Duration::from_millis(1);
        for _ in 0..3 {
            lockout::record_failure(&table, "carol", short);
        }
        std::thread::sleep(Duration::from_millis(5));
        assert!(!lockout::is_locked(&table, "carol", 3, short));

        // `lockout_after = 0` means the feature is off, not "always locked".
        lockout::record_failure(&table, "dave", window);
        assert!(!lockout::is_locked(&table, "dave", 0, window));
    }

    #[test]
    fn the_dummy_hash_verifies_nothing_but_parses() {
        let h = dummy_hash();
        assert!(h.starts_with("$argon2"));
        assert!(!verify_password("", h));
        assert!(!verify_password("password", h));
    }

    #[test]
    fn the_page_escapes_what_it_is_given() {
        // The page needs a Config only for the agent name; a minimal one is
        // enough to see that nothing typed lands in the HTML unescaped.
        let html = page_html("Thetis <x>", Some("Wrong \"user\""), "/a?b=<c>");
        assert!(html.contains("Thetis &lt;x&gt;"));
        assert!(html.contains("Wrong &quot;user&quot;"));
        assert!(html.contains("value=\"/a?b=&lt;c&gt;\""));
        assert!(!html.contains("<x>"));
        let quiet = page_html("T", None, "");
        assert!(!quiet.contains("role=\"alert\""), "no banner without a message");
        assert!(quiet.contains("value=\"/\""), "an empty next goes home");
    }

    #[test]
    fn a_principal_describes_itself_consistently() {
        use crate::policy::{Cap, EffectivePolicy};
        let mut policy = EffectivePolicy::unrestricted(&[], "m", &[], "agent", 2);
        policy.admin = false;
        policy.denied.insert(Cap::WorkspaceWrite);
        policy.denied.insert(Cap::Terminal);
        policy.see_all_sessions = true;
        let p = Principal::new("bob".into(), "Bob".into(), "dev".into(), Arc::new(policy));
        let d = p.describe();
        assert_eq!(d["id"], "bob");
        assert_eq!(d["admin"], false);
        assert_eq!(d["workspace"], "read");
        assert_eq!(d["see_all"], true);
        assert_eq!(d["viewing_all"], false, "off until the toggle is used");
        assert_eq!(p.list_owner(), Some("bob"));
        p.set_view_all(true);
        assert_eq!(p.describe()["viewing_all"], true);
        assert_eq!(p.list_owner(), None);
        let denied: Vec<String> = d["denied"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(denied.contains(&"terminal".to_string()));
        assert!(denied.contains(&"workspace_write".to_string()));
        assert!(!denied.contains(&"workspace".to_string()));
        assert_eq!(d["local"], false);

        // Someone without the grant cannot toggle their way past it.
        let mut narrow = EffectivePolicy::unrestricted(&[], "m", &[], "agent", 2);
        narrow.see_all_sessions = false;
        let p = Principal::new("eve".into(), "Eve".into(), "dev".into(), Arc::new(narrow));
        p.set_view_all(true);
        assert!(!p.viewing_all());
        assert_eq!(p.list_owner(), Some("eve"));
    }
}
