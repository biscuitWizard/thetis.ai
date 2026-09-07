//! Shared GitHub App client: JWT minting, installation tokens, REST plumbing.
//!
//! This file is duplicated verbatim into every `github-*` tool crate, exactly
//! as `notion.rs` is duplicated across the notion group. Each tool is a
//! standalone cargo package built for wasm32-wasip2 — there is no workspace to
//! hold a common library — so the copies are the price of the component
//! boundary. Keep them identical: edit one, copy to all.
//!
//! # Why a GitHub App rather than a bot user with a PAT
//!
//! An App is its own principal. It has a stable `[bot]` identity that authors
//! commits, issues and reviews under its own name; it can be installed on an
//! org or a single repo by someone with admin rights and removed the same way;
//! its permissions are fine-grained and auditable; and its credentials are
//! short-lived. A PAT on a human-shaped bot account inherits that account's
//! whole blast radius, needs a seat, and dies when the account does.
//!
//! # The two-step authentication dance
//!
//! 1. Sign a JWT with the App's RSA private key (`RS256`, `iss` = App id, life
//!    ≤ 10 minutes). This authenticates *as the App*, and can only reach
//!    `/app/*` endpoints — it cannot touch repository content.
//! 2. Exchange the JWT at `POST /app/installations/{id}/access_tokens` for an
//!    **installation access token**, which expires in one hour and *is* what
//!    reaches repositories. This is also the password for git over HTTPS.
//!
//! Step 2 is cached in the host KV store, because minting is a network round
//! trip and the token is good for an hour. See `installation_token`.
//!
//! TLS is terminated by the host, so nothing here needs a crypto stack for the
//! socket. The RSA signing is pure computation and builds fine for wasm.

#![allow(dead_code)] // Each tool uses a subset of this module.

// The bindings are generated at the crate root by `wit_bindgen::generate!`.
use crate::thetis::grip::sys;
use crate::thetis::grip::types::LogLevel;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::Duration;

pub const API_BASE: &str = "https://api.github.com";

/// Sent as `X-GitHub-Api-Version` on every request. Pinned deliberately: GitHub
/// dates its REST breaking changes, and a floating version would move the
/// ground under these tools.
pub const API_VERSION: &str = "2022-11-28";

/// GitHub requires a User-Agent and rejects requests without one.
pub const USER_AGENT: &str = "thetis-grip";

/// A JWT may live at most 10 minutes, and GitHub measures that as `exp - iat`.
/// Since `iat` is backdated by `JWT_BACKDATE_SECS`, the real span is this plus
/// the backdate — so eight minutes here totals nine, staying clear of the
/// ceiling rather than sitting exactly on it.
const JWT_LIFETIME_SECS: u64 = 8 * 60;

/// GitHub advises backdating `iat` to tolerate a fast local clock, which is
/// otherwise a confusing intermittent 401.
const JWT_BACKDATE_SECS: u64 = 60;

/// Installation tokens last an hour. Refresh with five minutes to spare so a
/// long tool call cannot straddle the expiry.
const TOKEN_REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

/// Output longer than this is cut with a note. The host truncates tool output at
/// 32 KiB anyway; cutting here means we can say *why*.
pub const MAX_OUTPUT_CHARS: usize = 18_000;

/// How the client is authenticating. Which one is in play changes what is
/// possible, so it is worth being able to say.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// A GitHub App: JWT -> installation token. The good path.
    App,
    /// A bare token from config (PAT or pre-minted). No `/app/*` endpoints, and
    /// commits are authored by whoever owns the token.
    Token,
}

/// A configured client. Cheap to construct; holds no connection.
pub struct GitHub {
    auth: Auth,
    /// App id (numeric) or client id (`Iv1.`/`Iv23` form). Both work as `iss`.
    app_id: String,
    private_key_pem: String,
    /// Pinned installation, when configured. Otherwise discovered on demand.
    installation_id: Option<u64>,
    /// A bare token, for the `Auth::Token` path.
    static_token: Option<String>,
    timeout: Duration,
}

impl GitHub {
    /// Builds a client from the tool's own config block.
    ///
    /// Every `github-*` tool inherits `[tools.github]`, so the credential is
    /// named once rather than once per tool.
    pub fn from_config(config_json: &str) -> Result<Self, String> {
        let config: Value = serde_json::from_str(config_json).unwrap_or(json!({}));

        let timeout = Duration::from_secs(
            config
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .clamp(5, 120),
        );

        let app_id = string_field(&config, &["app_id", "client_id"]).unwrap_or_default();
        let private_key_pem = private_key(&config);

        // A bare token is the fallback, not the preference — but if that is all
        // that is configured, working beats refusing.
        let static_token = string_field(&config, &["token", "pat", "access_token"]);

        if !app_id.is_empty() && private_key_pem.is_some() {
            return Ok(Self {
                auth: Auth::App,
                app_id,
                private_key_pem: private_key_pem.unwrap(),
                installation_id: config.get("installation_id").and_then(as_u64_loose),
                static_token,
                timeout,
            });
        }

        if let Some(token) = static_token {
            return Ok(Self {
                auth: Auth::Token,
                app_id: String::new(),
                private_key_pem: String::new(),
                installation_id: None,
                static_token: Some(token),
                timeout,
            });
        }

        // Say which half is missing: "no credential" when the app id is present
        // but the key is not sends the reader looking in the wrong place.
        Err(match (app_id.is_empty(), private_key_pem.is_none()) {
            (false, true) => format!(
                "app_id is set but no private key is. Add `private_key_path` (or \
                 `private_key`) to [tools.github].\n\n{SETUP_HELP}"
            ),
            (true, false) => format!(
                "a private key is set but app_id is not. Add `app_id` to \
                 [tools.github].\n\n{SETUP_HELP}"
            ),
            _ => format!("no GitHub credentials configured.\n\n{SETUP_HELP}"),
        })
    }

    pub fn auth_kind(&self) -> Auth {
        self.auth
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn configured_installation(&self) -> Option<u64> {
        self.installation_id
    }

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    /// Signs a JWT authenticating as the App itself.
    ///
    /// Only `/app/*` endpoints accept this. Repository calls need the
    /// installation token that this is exchanged for.
    pub fn app_jwt(&self) -> Result<String, String> {
        if self.auth != Auth::App {
            return Err("this operation needs GitHub App credentials (app_id + private key), \
                        not a bare token."
                .to_string());
        }

        let now = sys::now_ms() / 1000;
        let iat = now.saturating_sub(JWT_BACKDATE_SECS);
        let exp = now + JWT_LIFETIME_SECS;

        let header = json!({ "alg": "RS256", "typ": "JWT" });
        let claims = json!({ "iat": iat, "exp": exp, "iss": self.app_id });

        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );

        let key = parse_private_key(&self.private_key_pem)?;
        // PKCS#1 v1.5 signing is deterministic, so no RNG is needed — which
        // matters, because entropy in a wasm guest is not a given.
        let signing_key = SigningKey::<Sha256>::new(key);
        let signature = signing_key.sign(signing_input.as_bytes());

        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    /// An installation access token, minted if the cached one is stale.
    ///
    /// Cached in the host KV store under the installation id: minting costs a
    /// network round trip and the result is valid for an hour, so a tool chain
    /// of six calls should pay for it once rather than six times.
    pub fn installation_token(&self) -> Result<String, String> {
        if self.auth == Auth::Token {
            return self
                .static_token
                .clone()
                .ok_or_else(|| "no token configured".to_string());
        }

        let installation_id = self.resolve_installation()?;
        let cache_key = format!("github/installation-token/{}/{}", self.app_id, installation_id);

        if let Some(cached) = sys::kv_get("global", &cache_key) {
            if let Ok(entry) = serde_json::from_str::<Value>(&cached) {
                let expires_at = entry.get("expires_at_ms").and_then(Value::as_u64).unwrap_or(0);
                let token = entry.get("token").and_then(Value::as_str).unwrap_or("");
                if !token.is_empty() && expires_at > sys::now_ms() + TOKEN_REFRESH_MARGIN_MS {
                    return Ok(token.to_string());
                }
            }
        }

        let jwt = self.app_jwt()?;
        let response = self.raw(
            "POST",
            &format!("/app/installations/{installation_id}/access_tokens"),
            &[],
            None,
            &format!("Bearer {jwt}"),
        )?;

        let token = response
            .get("token")
            .and_then(Value::as_str)
            .ok_or("GitHub did not return a token for this installation")?
            .to_string();

        // GitHub reports the expiry as ISO-8601. Rather than parse it, trust
        // the documented one-hour lifetime from now — always an underestimate,
        // which is the safe direction for a cache.
        let entry = json!({
            "token": token,
            "expires_at_ms": sys::now_ms() + 60 * 60 * 1000,
        });
        sys::kv_put("global", &cache_key, &entry.to_string());

        Ok(token)
    }

    /// The installation to act through: the configured one, or the only one
    /// there is.
    ///
    /// Guessing is safe when there is exactly one installation and dangerous
    /// when there are several, so with several this refuses and names them —
    /// writing to the wrong org is not an error worth recovering from.
    pub fn resolve_installation(&self) -> Result<u64, String> {
        if let Some(id) = self.installation_id {
            return Ok(id);
        }

        let installations = self.list_installations()?;
        match installations.len() {
            0 => Err(format!(
                "the GitHub App has no installations, so it cannot reach any repository. \
                 Install it on an account or org first.\n\n{INSTALL_HELP}"
            )),
            1 => installations[0]
                .get("id")
                .and_then(as_u64_loose)
                .ok_or_else(|| "installation has no id".to_string()),
            _ => {
                let mut names = String::new();
                for installation in &installations {
                    let id = installation
                        .get("id")
                        .and_then(as_u64_loose)
                        .unwrap_or_default();
                    names.push_str(&format!("\n  {id}  {}", account_of(installation)));
                }
                Err(format!(
                    "the App has {} installations, so which one to act through is ambiguous. \
                     Set `installation_id` in [tools.github]:{names}",
                    installations.len()
                ))
            }
        }
    }

    /// Every account this App is installed on.
    pub fn list_installations(&self) -> Result<Vec<Value>, String> {
        let jwt = self.app_jwt()?;
        let response = self.raw(
            "GET",
            "/app/installations",
            &[("per_page".to_string(), "100".to_string())],
            None,
            &format!("Bearer {jwt}"),
        )?;
        Ok(response.as_array().cloned().unwrap_or_default())
    }

    /// The App's own record: slug, name, permissions.
    pub fn app_info(&self) -> Result<Value, String> {
        let jwt = self.app_jwt()?;
        self.raw("GET", "/app", &[], None, &format!("Bearer {jwt}"))
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    pub fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value, String> {
        self.send("GET", path, query, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("POST", path, &[], Some(body))
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("PATCH", path, &[], Some(body))
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("PUT", path, &[], Some(body))
    }

    pub fn delete(&self, path: &str) -> Result<Value, String> {
        self.send("DELETE", path, &[], None)
    }

    /// One request authenticated as the installation.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let token = self.installation_token()?;
        self.raw(method, path, query, body, &format!("Bearer {token}"))
    }

    /// Walks a paginated endpoint until it runs dry or `max` items are
    /// collected, whichever comes first.
    pub fn paginate(
        &self,
        path: &str,
        query: &[(String, String)],
        max: usize,
    ) -> Result<Vec<Value>, String> {
        let mut collected: Vec<Value> = Vec::new();
        let mut page = 1u32;

        while collected.len() < max {
            let want = (max - collected.len()).min(100);
            let mut q = query.to_vec();
            q.push(("per_page".to_string(), want.to_string()));
            q.push(("page".to_string(), page.to_string()));

            let response = self.get(path, &q)?;
            // Some endpoints return a bare array, others wrap it in `items`.
            let items = match &response {
                Value::Array(items) => items.clone(),
                other => other
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            };

            let count = items.len();
            collected.extend(items);
            if count < want {
                break;
            }
            page += 1;
        }

        Ok(collected)
    }

    /// The one place an HTTP request is actually made.
    fn raw(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        authorization: &str,
    ) -> Result<Value, String> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{API_BASE}{path}")
        };
        sys::log(LogLevel::Debug, &format!("github: {method} {path}"));

        let client = waki::Client::new();
        let mut request = match method {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PATCH" => client.patch(&url),
            "PUT" => client.put(&url),
            "DELETE" => client.delete(&url),
            other => return Err(format!("unsupported HTTP method {other:?}")),
        };

        // An empty string means send no Authorization header at all. Some
        // endpoints are public and actively reject an App JWT, so "no
        // credential" has to be expressible rather than approximated.
        if !authorization.is_empty() {
            request = request.header("Authorization", authorization);
        }

        request = request
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/json")
            .connect_timeout(self.timeout);

        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.body(body.to_string().into_bytes());
        }

        let response = request
            .send()
            .map_err(|e| format!("could not reach api.github.com: {e}"))?;

        let status = response.status_code();
        let bytes = response
            .body()
            .map_err(|e| format!("could not read GitHub's response: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).to_string();

        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(json!({}));
            }
            return serde_json::from_str(&text)
                .map_err(|e| format!("GitHub's response was not JSON: {e}: {}", clip(&text, 300)));
        }

        Err(explain_error(status, &text))
    }
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

/// Reads the private key from config, accepting both an inline PEM and a path.
///
/// A path is by far the better habit — the key is multi-line and a TOML string
/// holding it is easy to mangle — but a downloaded `.pem` pasted inline should
/// still work rather than failing obscurely.
fn private_key(config: &Value) -> Option<String> {
    if let Some(inline) = string_field(config, &["private_key", "private_key_pem", "pem"]) {
        // TOML basic strings turn a real newline into the two characters `\n`.
        // Repairing that here costs one line and removes a whole class of
        // baffling "invalid PEM" reports.
        return Some(inline.replace("\\n", "\n"));
    }

    // A tool has no filesystem import, so the host reads a `*_path` secret on
    // our behalf and inlines it as `*_contents`. See `inline_file_secrets` in
    // crates/thetis/src/config.rs.
    if let Some(contents) = string_field(config, &["private_key_contents", "pem_contents"]) {
        return Some(contents);
    }

    // The path was set but unreadable: the host says why, and carrying that
    // reason through beats reporting the key as simply absent.
    let path = string_field(config, &["private_key_path", "pem_path", "key_path"])?;
    let reason = string_field(config, &["private_key_contents_error", "pem_contents_error"])
        .unwrap_or_else(|| "the file could not be read".to_string());
    Some(format!("@@UNREADABLE@@{path}: {reason}"))
}

/// Parses a PEM private key in either of the two forms GitHub hands out.
///
/// The download from the App settings page is PKCS#1 (`BEGIN RSA PRIVATE KEY`).
/// Anyone who has run it through `openssl pkcs8` has PKCS#8 (`BEGIN PRIVATE
/// KEY`). Accept both, because telling them apart is not the user's job.
fn parse_private_key(pem: &str) -> Result<RsaPrivateKey, String> {
    if let Some(detail) = pem.strip_prefix("@@UNREADABLE@@") {
        // The host has already said why it could not read the file, and the
        // reasons need different fixes: a missing file is a typo or a key that
        // was never saved, whereas a rejected one is outside the project root.
        // Only add the confinement advice when that is actually the problem.
        let hint = if detail.contains("inside the project root") {
            "The path must stay inside the project root. Move the .pem under the project \
             (secrets/ is a good choice) or paste it inline as `private_key`."
        } else {
            "Check the path, which is resolved against the project root. Download the key \
             from the App's settings page if you do not have it, or paste it inline as \
             `private_key`."
        };
        return Err(format!("could not read the private key: {detail}\n\n{hint}"));
    }

    let pem = pem.trim();
    if !pem.contains("PRIVATE KEY") {
        return Err("the configured private key is not PEM. It should begin with \
                    `-----BEGIN RSA PRIVATE KEY-----`, as downloaded from the App's \
                    settings page."
            .to_string());
    }

    RsaPrivateKey::from_pkcs1_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs8_pem(pem))
        .map_err(|e| {
            format!(
                "the private key could not be parsed: {e}. GitHub issues a PKCS#1 PEM \
                 (`BEGIN RSA PRIVATE KEY`); PKCS#8 (`BEGIN PRIVATE KEY`) also works. \
                 If the key was pasted into TOML, prefer `private_key_path` instead."
            )
        })
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The git author/committer identity for a GitHub App's bot user.
///
/// GitHub attributes a commit to the App when the committer email matches
/// `<bot-user-id>+<slug>[bot]@users.noreply.github.com`. Get this wrong and the
/// commit shows up as an unlinked stranger, which is the single most common
/// mistake in App-authored commits — the id is the *bot user's*, not the App's.
pub struct Identity {
    pub slug: String,
    pub name: String,
    pub email: String,
    pub bot_user_id: u64,
}

impl GitHub {
    /// Resolves the App's bot identity, cached because it never changes.
    pub fn identity(&self) -> Result<Identity, String> {
        if self.auth == Auth::Token {
            return Err("a bare token has no [bot] identity; commits will be authored by the \
                        token's owner. Configure App credentials for a distinct identity."
                .to_string());
        }

        let cache_key = format!("github/identity/{}", self.app_id);
        if let Some(cached) = sys::kv_get("global", &cache_key) {
            if let Ok(entry) = serde_json::from_str::<Value>(&cached) {
                if let (Some(slug), Some(id)) = (
                    entry.get("slug").and_then(Value::as_str),
                    entry.get("bot_user_id").and_then(Value::as_u64),
                ) {
                    return Ok(Identity {
                        slug: slug.to_string(),
                        name: format!("{slug}[bot]"),
                        email: bot_email(id, slug),
                        bot_user_id: id,
                    });
                }
            }
        }

        let app = self.app_info()?;
        let slug = app
            .get("slug")
            .and_then(Value::as_str)
            .ok_or("GitHub's /app response had no slug")?
            .to_string();

        // The bot user is a separate account from the App, with its own id.
        //
        // Send this one *unauthenticated*. `/users/{username}` is public, and an
        // App JWT is only accepted on `/app/*` endpoints — presenting one here
        // earns a 401 "Bad credentials", which looks alarmingly like a broken
        // key when in fact the key is fine and merely out of scope.
        let bot = self
            .raw("GET", &format!("/users/{slug}%5Bbot%5D"), &[], None, "")
            .map_err(|e| {
                format!("could not look up the bot user for {slug}[bot]: {e}")
            })?;
        let bot_user_id = bot
            .get("id")
            .and_then(as_u64_loose)
            .ok_or("could not resolve the bot user's id")?;

        sys::kv_put(
            "global",
            &cache_key,
            &json!({ "slug": slug, "bot_user_id": bot_user_id }).to_string(),
        );

        Ok(Identity {
            name: format!("{slug}[bot]"),
            email: bot_email(bot_user_id, &slug),
            slug,
            bot_user_id,
        })
    }
}

pub fn bot_email(bot_user_id: u64, slug: &str) -> String {
    format!("{bot_user_id}+{slug}[bot]@users.noreply.github.com")
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

// Naming the file precisely matters more than it looks. Thetis runs each
// conversation from a git worktree, and there are two overlays: the worktree's
// own thetis.local.toml, and the shared one named by THETIS_LOCAL_CONFIG.
// Both are read, but the shared one is merged last and therefore wins. Put a
// long-lived credential in the shared file so it survives branch switches.
pub const SETUP_HELP: &str = "Set up a GitHub App, then add this to \
thetis.local.toml, which is gitignored. Prefer the shared one -- the file \
named by THETIS_LOCAL_CONFIG, normally <project>/thetis.local.toml at the top \
level -- because it applies to every branch and is merged last, so it wins over \
a copy inside a worktree:\n\
\n\
[tools.github]\n\
app_id = \"123456\"\n\
private_key_path = \"secrets/github-app.pem\"\n\
# installation_id = 12345678   # only needed if the App has several\n\
\n\
private_key_path is resolved against the project root and must stay inside it; \
the host reads the file and passes the contents in. `private_key` inline works \
too.\n\
\n\
Then restart the orchestrator. Every github-* tool inherits this block, so the \
credential is set once. Run github-whoami to verify it.";

pub const INSTALL_HELP: &str = "Install the App from \
https://github.com/settings/apps/<your-app>/installations — choose the account \
or org, and either all repositories or a chosen few.";

/// GitHub's error body carries a `message` and often a documentation URL. The
/// status is what to branch on; the message is what to show.
fn explain_error(status: u16, text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).unwrap_or(json!({}));
    let message = parsed
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text.trim());

    let lower = message.to_ascii_lowercase();

    let hint = match status {
        // "could not be decoded" is specifically a malformed JWT — a wrong
        // app_id or a mangled key — as opposed to a well-formed token GitHub
        // simply does not recognise. Worth separating: the fixes differ.
        401 if lower.contains("could not be decoded") => Some(
            "GitHub could not decode the JWT. The signature or claims are malformed: check \
             that app_id is the App's own numeric id (not the installation id) and that the \
             private key belongs to that same App.",
        ),
        401 => Some(
            "GitHub rejected the credential. For App auth this is usually a bad private key, \
             an app_id that is not the App's own id, or a clock more than a minute fast.",
        ),
        403 if lower.contains("rate limit") => Some(
            "Rate limited. An installation token gets 5,000 requests an hour; wait for the \
             window to reset.",
        ),
        403 => Some(
            "The App is installed but lacks the permission this call needs. Add it under the \
             App's Permissions, then accept the request on the installation — GitHub does not \
             grant new permissions to an existing installation until it is approved.",
        ),
        404 => Some(
            "Either the path is wrong, or the App is not installed on this repository. A 404 \
             rather than a 403 is how GitHub hides things the credential cannot see, so an \
             unlisted repo looks identical to a missing one.",
        ),
        409 => Some("The repository is empty, or a ref moved under the request. Retry."),
        422 => Some(
            "GitHub accepted the shape but refused the content: a ref that already exists, a \
             PR with no diff, or a branch name that is taken.",
        ),
        _ => None,
    };

    let mut out = format!("GitHub returned {status}: {}", clip(message, 600));
    if let Some(errors) = parsed.get("errors") {
        out.push_str(&format!("\nerrors: {}", clip(&errors.to_string(), 400)));
    }
    if let Some(hint) = hint {
        out.push_str(&format!("\n\n{hint}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn string_field(config: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| config.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(str::to_string)
}

/// An id that may arrive as a number or a string. TOML makes it easy to quote a
/// numeric id, and refusing that would be pedantry.
fn as_u64_loose(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The account an installation belongs to, for a human-readable listing.
pub fn account_of(installation: &Value) -> String {
    installation
        .get("account")
        .and_then(|a| {
            a.get("login")
                .or_else(|| a.get("slug"))
                .and_then(Value::as_str)
        })
        .unwrap_or("(unknown account)")
        .to_string()
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}... [cut]")
}

/// `owner/repo` from an argument that may be either that or a full URL.
///
/// A model reliably has a URL to hand and only sometimes has the short form, so
/// accepting both removes a failure mode rather than adding a nicety.
pub fn parse_repo(raw: &str) -> Result<(String, String), String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("missing required argument 'repo'".to_string());
    }

    let core = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");

    // Strip any scheme/host prefix, keeping the last two path segments.
    let without_scheme = core
        .rsplit_once("github.com/")
        .map(|(_, rest)| rest)
        .unwrap_or(core)
        .trim_start_matches('/');

    let parts: Vec<&str> = without_scheme.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err(format!(
            "{raw:?} is not a repository. Give `owner/repo` or the repository URL."
        ));
    }

    Ok((parts[0].to_string(), parts[1].to_string()))
}
