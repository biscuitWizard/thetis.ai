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
        // Administrators land on the installation-wide conversation list. They
        // can still use the sidebar control to narrow the tab back to their own.
        let view_all = policy.admin;
        Self {
            user_id,
            display_name,
            role,
            policy,
            view_all: Arc::new(AtomicBool::new(view_all)),
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
        self.is_admin() || self.policy.see_all_sessions
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
    if !g.cfg().auth.users_mode {
        return Some(Principal::local(&g.cfg()));
    }
    let hash = token_hash(&cookie_value(h)?);
    let st = g.local_store()?;
    let row = st.get_login(&hash).ok().flatten()?;
    let now = crate::store::now_ms();
    if row.expires_ms <= now {
        let _ = st.remove_login(&hash);
        return None;
    }
    let cfg = g.cfg();
    let Some(u) = cfg.auth.user(&row.user_id) else {
        let _ = st.remove_login(&hash);
        return None;
    };
    if now.saturating_sub(row.last_seen_ms) > 60_000 {
        let _ = st.touch_login(&hash, now, now + g.cfg().auth.session_ttl.as_millis() as u64);
    }
    Some(Principal::from_user(u))
}
/// Whether this principal may see and act on a conversation.
///
/// Three ways in, checked in this order: a blanket grant, ownership, and an
/// invitation. The invitation check is last of the three that can succeed
/// because it is the only one that touches a second table, and the common case
/// is a conversation of one's own.
///
/// Being a participant grants *access*, never *authority*: what a participant's
/// turn may do is `policy(speaker) ∩ ceiling(session)`, resolved separately in
/// `store::session_policy`. That is what makes an invitation safe to hand out —
/// it cannot lend the invitee any of the owner's capabilities.
pub fn may_access(g: &Grip, p: &Principal, id: &str) -> Result<()> {
    // Administrators have the blanket grant intrinsically. Checking the raw
    // see-all bit here disagreed with `Principal::may_see_all`: a custom admin
    // role could list foreign conversations, then be refused when opening or
    // archiving the exact row it had just been shown.
    if p.may_see_all() {
        return Ok(());
    }
    let st = g.local_store().context("ownership is gateway-only")?;
    match st.owner_of_root(id)? {
        Some(o) if o == p.user_id => Ok(()),
        Some(_) => {
            if st.is_participant(id, &p.user_id)? {
                Ok(())
            } else {
                // The same message either way: distinguishing "not yours" from
                // "does not exist" tells an outsider which ids are real.
                bail!("conversation belongs to another user")
            }
        }
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
    axum::response::Html(page_html(&c.agent_name, &c.agent_avatar, msg, next))
}

fn page_html(agent_name: &str, agent_avatar: &str, msg: Option<&str>, next: &str) -> String {
    let banner = msg
        .map(|m| format!(r#"<p class="banner" role="alert">{}</p>"#, html_escape(m)))
        .unwrap_or_default();
    // The same face as the app's header: the configured picture when there is
    // one (a URL or a data: URI, both fine in a `src`), otherwise the mark.
    // Only `http(s):` and `data:image/` are let through, so a stray value in
    // the config cannot become a `javascript:` URL on the one page that has
    // no CSP of its own.
    let avatar_ok = agent_avatar.starts_with("https://")
        || agent_avatar.starts_with("http://")
        || agent_avatar.starts_with("data:image/");
    let brand_mark = if avatar_ok {
        format!(
            r#"<img class="mark" src="{}" alt="" width="36" height="36" decoding="async">"#,
            html_escape(agent_avatar)
        )
    } else {
        r#"<svg class="mark" viewBox="0 0 32 32" aria-hidden="true"><circle cx="16" cy="16" r="9" fill="none" stroke="currentColor" stroke-width="2.5"/><circle cx="16" cy="16" r="3" fill="currentColor"/></svg>"#.to_string()
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>{name} — sign in</title>
<style>
:root{{--bg:#0b0b0f;--surface:#101016;--surface-2:#16161f;--surface-3:#1d1d28;--hairline:#24242f;--hairline-strong:#32323f;--text:#ececf2;--text-dim:#a3a3b4;--text-faint:#6e6e82;--accent:#7c9cff;--accent-hot:#96b0ff;--accent-deep:#4a5fd0;--err:#f2788f;--err-wash:rgba(242,120,143,.12);--glow:rgba(124,156,255,.18)}}
@media (prefers-color-scheme:light){{:root{{--bg:#f5f5f8;--surface:#ffffff;--surface-2:#f1f1f6;--surface-3:#e6e6ee;--hairline:#dedee6;--hairline-strong:#c8c8d4;--text:#17171f;--text-dim:#565669;--text-faint:#8a8a9c;--accent:#4a5fd0;--accent-hot:#3646b3;--accent-deep:#4a5fd0;--err:#c0384f;--err-wash:rgba(192,56,79,.10);--glow:rgba(74,95,208,.14)}}}}
*{{box-sizing:border-box}}
html,body{{height:100%}}
body{{margin:0;display:grid;place-items:center;padding:1.5rem;background:var(--bg);background-image:radial-gradient(60rem 30rem at 50% -10%,var(--glow),transparent 60%);color:var(--text);font:15px/1.6 "Inter",ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;-webkit-font-smoothing:antialiased}}
main{{width:100%;max-width:24rem;padding:2rem 2rem 1.5rem;background:var(--surface);border:1px solid var(--hairline);border-radius:16px;box-shadow:0 1px 2px rgba(0,0,0,.25),0 24px 60px -20px rgba(0,0,0,.5)}}
.brand{{display:flex;flex-direction:column;align-items:center;gap:.7rem;margin:0 0 1.4rem;text-align:center}}
.mark{{width:44px;height:44px;border-radius:12px;color:var(--accent);background:var(--surface-2);border:1px solid var(--hairline);padding:6px;object-fit:cover}}
img.mark{{padding:0}}
h1{{font-size:1.2rem;font-weight:600;margin:0;letter-spacing:-.01em;max-width:100%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.lead{{margin:0;color:var(--text-dim);font-size:.88rem}}
label{{display:block;margin:1rem 0 .35rem;color:var(--text-dim);font-size:.72rem;font-weight:600;text-transform:uppercase;letter-spacing:.08em}}
input{{font:inherit;width:100%;padding:.6rem .75rem;border-radius:9px;border:1px solid var(--hairline-strong);background:var(--surface-2);color:var(--text);outline:none;transition:border-color .12s,box-shadow .12s,background .12s}}
input:hover{{border-color:var(--text-faint)}}
input:focus{{border-color:var(--accent);background:var(--surface);box-shadow:0 0 0 3px var(--glow)}}
button{{font:inherit;font-weight:600;width:100%;margin-top:1.4rem;padding:.65rem .9rem;border-radius:9px;border:1px solid transparent;background:var(--accent-deep);color:#fff;cursor:pointer;transition:background .12s,transform .06s}}
button:hover{{background:var(--accent-hot)}}
button:active{{transform:translateY(1px)}}
button:focus-visible{{outline:2px solid var(--accent-hot);outline-offset:2px}}
.banner{{margin:0 0 .25rem;padding:.6rem .8rem;border-radius:9px;font-size:.88rem;color:var(--err);background:var(--err-wash);border:1px solid color-mix(in srgb,var(--err) 35%,transparent)}}
.foot{{margin:1.4rem 0 0;color:var(--text-faint);font-size:.78rem;text-align:center;line-height:1.5}}
</style>
<main>
  <div class="brand">
    {brand_mark}
    <h1 title="{name}">{name}</h1>
    <p class="lead">Sign in to continue</p>
  </div>
  {banner}
  <form method="post" action="/login">
    <input type="hidden" name="next" value="{next}">
    <label for="user">User</label>
    <input id="user" name="user" autocomplete="username" autocapitalize="none" spellcheck="false" required autofocus>
    <label for="password">Password</label>
    <input id="password" name="password" type="password" autocomplete="current-password" required>
    <button type="submit">Sign in</button>
  </form>
  <p class="foot">No account? Ask whoever runs this Thetis.</p>
</main>
"#,
        name = html_escape(agent_name),
        brand_mark = brand_mark,
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
        let html = page_html("Thetis <x>", "", Some("Wrong \"user\""), "/a?b=<c>");
        assert!(html.contains("Thetis &lt;x&gt;"));
        assert!(html.contains("Wrong &quot;user&quot;"));
        assert!(html.contains("value=\"/a?b=&lt;c&gt;\""));
        assert!(!html.contains("<x>"));
        let quiet = page_html("T", "", None, "");
        assert!(!quiet.contains("role=\"alert\""), "no banner without a message");
        assert!(quiet.contains("value=\"/\""), "an empty next goes home");
        assert!(quiet.contains("<svg class=\"mark\""), "no avatar means the mark");
        // The avatar is used when it is a picture, and never when it is a script.
        let pic = page_html("T", "data:image/png;base64,AAAA", None, "");
        assert!(pic.contains("<img class=\"mark\" src=\"data:image/png;base64,AAAA\""));
        let bad = page_html("T", "javascript:alert(1)", None, "");
        assert!(!bad.contains("javascript:"));
        assert!(bad.contains("<svg class=\"mark\""));
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
        assert_eq!(d["viewing_all"], false, "a non-admin starts personal");
        assert_eq!(p.list_owner(), Some("bob"));
        p.set_view_all(true);
        assert_eq!(p.describe()["viewing_all"], true);
        assert_eq!(p.list_owner(), None);
        let denied: Vec<String> = d["denied"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(denied.contains(&"terminal".to_string()));
        assert!(denied.contains(&"workspace_write".to_string()));
        assert!(!denied.contains(&"workspace".to_string()));
        assert_eq!(d["local"], false);

        // Admins start on the installation-wide list without having to opt in,
        // even when a custom admin role omitted the narrower see-all grant.
        let mut admin_policy = EffectivePolicy::unrestricted(&[], "m", &[], "agent", 2);
        admin_policy.see_all_sessions = false;
        let admin = Principal::new(
            "ada".into(),
            "Ada".into(),
            "admin".into(),
            Arc::new(admin_policy),
        );
        assert!(admin.may_see_all());
        assert_eq!(admin.list_owner(), None);

        // Someone without the grant cannot toggle their way past it.
        let mut narrow = EffectivePolicy::unrestricted(&[], "m", &[], "agent", 2);
        narrow.admin = false;
        narrow.see_all_sessions = false;
        let p = Principal::new("eve".into(), "Eve".into(), "dev".into(), Arc::new(narrow));
        p.set_view_all(true);
        assert!(!p.viewing_all());
        assert_eq!(p.list_owner(), Some("eve"));
    }
}
