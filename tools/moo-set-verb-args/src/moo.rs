//! Shared mooR web-host client, config and error helpers.
//!
//! This file is the canonical copy of the shared client for the `moo-*` tool
//! family. If more `moo-*` tools are added later, copy this file into each new
//! crate with `sync-shared-client.sh` and never edit a copy directly — see
//! that script's header for why duplication (not a shared library) is the
//! right trade for a standalone wasm component.
//!
//! ## What this covers
//!
//! `crates/web-host` in the mooR repository (`/opt/thetis/workspace/moor`) is
//! the protocol authority this was written against. Three endpoints need no
//! authentication and are what a server-info tool can responsibly poll:
//!
//! * `GET /health` — 200 with an empty body when the web host has heard from
//!   its daemon within the last 30 seconds, 503 otherwise. Never any body.
//! * `GET /version` — `{"version": "...", "commit": "..."}` as JSON.
//! * `GET /v1/features` — content-negotiated between
//!   `application/x-flatbuffers` (the default with no `Accept` header) and
//!   `application/json`. Sending `Accept: application/json` explicitly is
//!   required, exactly as `moor/services/clients-and-web-ui` warns: relying on
//!   the default gets a FlatBuffers binary blob back, and asking for neither
//!   format gets `406 Not Acceptable`.
//!
//! Everything else under `/v1/...` needs an `X-Moor-Auth-Token` from
//! `/auth/connect` or `/auth/create` and is out of scope for a read-only
//! status tool.
//!
//! Confirmed against a live server (`http://10.10.10.1:7892`, mooR
//! `2.0.0-dev`) while writing this:
//!
//! ```text
//! GET /health                        -> 200, empty body
//! GET /version                       -> 200 {"version":"2.0.0-dev","commit":"VERGEN_"}
//! GET /v1/features (Accept: json)    -> 200 {"result":{"HostSuccess":{"reply":{"reply":
//!                                        {"ServerFeatures":{"persistent_tasks":true,...}}}}}}
//! GET /v1/features (no Accept)       -> 200, application/x-flatbuffers (binary, unusable here)
//! GET /v1/features (Accept: text/xml)-> 406 Not Acceptable, empty body
//! GET /nonexistent                   -> 404, empty body
//! ```
//!
//! The `/v1/features` JSON body is several FlatBuffers union layers deep —
//! `ReplyResult::HostSuccess -> DaemonToHostReply -> ServerFeatures` — because
//! the web host serializes the same wire enum it uses for the ZeroMQ RPC
//! protocol (see `moor-schema`'s `ReplyResultUnion`, `DaemonToHostReplyUnion`
//! in `crates/schema/schema/moor_rpc.fbs`). [`server_features`] unwraps that
//! nesting so a caller gets the flat feature map directly, and degrades to
//! `None` rather than failing if a future server nests it differently — the
//! `clients-and-web-ui` skill's rule that an evolving reply shape must be
//! tolerated, not treated as fatal.

#![allow(dead_code)] // Some helpers exist for tools not yet written in this family.

use crate::thetis::grip::sys;
use crate::thetis::grip::types::LogLevel;
use serde_json::{json, Value};
use std::time::Duration;

/// The address the expected live deployment answers on. Used only when
/// nothing in config says otherwise — never hardcoded into a request.
pub const DEFAULT_BASE_URL: &str = "http://10.10.10.1:7892";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A configured client. Cheap to construct; holds no connection.
pub struct Moo {
    pub base_url: String,
    pub username: Option<String>,
    password: Option<String>,
    pub wizard_username: Option<String>,
    wizard_password: Option<String>,
    pub auth_token: Option<String>,
    pub timeout: Duration,
    pub request_timeout: Duration,
}

impl Moo {
    /// Builds a client from this tool's own `[tools.moo*]` config block.
    ///
    /// The base URL is **only** ever taken from config (`base_url`, or `url`
    /// as an alias) — falling back to [`DEFAULT_BASE_URL`] when neither is
    /// set. There is deliberately no argument that lets a call override it:
    /// the whole point of resolving it through config is that one operator
    /// setting governs every call, and an argument would let a stray value
    /// silently point this at the wrong server.
    pub fn from_config(config_json: &str) -> Result<Self, String> {
        let config: Value = serde_json::from_str(config_json).unwrap_or(json!({}));
        Self::from_value(&config)
    }

    fn from_value(config: &Value) -> Result<Self, String> {
        let base_url = ["base_url", "url"]
            .iter()
            .filter_map(|k| config.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();

        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(format!(
                "'{base_url}' is not a valid base_url: it must start with http:// or https://. \
                 Set [tools.moo] base_url = \"http://host:port\" in thetis.toml."
            ));
        }

        let auth_token = ["auth_token", "token"]
            .iter()
            .filter_map(|k| config.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string);
        let field = |name: &str| {
            config.get(name).and_then(Value::as_str).map(str::trim)
                .filter(|s| !s.is_empty()).map(str::to_string)
        };
        let username = field("username");
        let password = field("password");
        let wizard_username = field("wizard_username");
        let wizard_password = field("wizard_password");
        if username.is_some() != password.is_some() {
            return Err("[tools.moo] username and password must be configured together".into());
        }
        if wizard_username.is_some() != wizard_password.is_some() {
            return Err("[tools.moo] wizard_username and wizard_password must be configured together".into());
        }

        let timeout = config
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 120);
        let request_timeout = config.get("request_timeout_secs").and_then(Value::as_u64)
            .unwrap_or(30).clamp(1, 300);

        Ok(Self {
            base_url,
            username,
            password,
            wizard_username,
            wizard_password,
            auth_token,
            timeout: Duration::from_secs(timeout),
            request_timeout: Duration::from_secs(request_timeout),
        })
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    /// `GET` with an explicit JSON `Accept` header. mooR's web host defaults
    /// to FlatBuffers when `Accept` is absent or `*/*` — see
    /// `negotiate_response_format` in `crates/web-host/src/host/negotiate.rs`
    /// — so a tool that wants JSON must always ask for it explicitly, never
    /// rely on the default.
    pub fn get_json(&self, path: &str) -> Result<HttpResponse, String> {
        self.request("GET", path, Some("application/json"))
    }

    /// `GET` with no `Accept` header at all, for an endpoint (`/health`,
    /// `/version`) that carries no format negotiation and always answers the
    /// same way.
    pub fn get_plain(&self, path: &str) -> Result<HttpResponse, String> {
        self.request("GET", path, None)
    }

    pub fn credentials(&self, wizard: bool) -> Result<(&str, &str), String> {
        let (user, pass, label) = if wizard {
            (self.wizard_username.as_deref(), self.wizard_password.as_deref(), "wizard")
        } else {
            (self.username.as_deref(), self.password.as_deref(), "programmer")
        };
        match (user, pass) {
            (Some(u), Some(p)) => Ok((u, p)),
            _ => Err(format!("missing {label} credentials in [tools.moo]")),
        }
    }

    fn cache_key(&self, wizard: bool) -> Result<String, String> {
        let (user, _) = self.credentials(wizard)?;
        Ok(format!("moo-auth:{}:{}", self.base_url, user))
    }

    pub fn clear_cached_token(&self, wizard: bool) -> Result<(), String> {
        sys::kv_put("global", &self.cache_key(wizard)?, "");
        Ok(())
    }

    pub fn token(&self, wizard: bool) -> Result<String, String> {
        if !wizard {
            if let Some(token) = &self.auth_token { return Ok(token.clone()); }
        }
        let key = self.cache_key(wizard)?;
        if let Some(token) = sys::kv_get("global", &key).filter(|t| !t.trim().is_empty()) {
            return Ok(token);
        }
        self.login(wizard)
    }

    pub fn login(&self, wizard: bool) -> Result<String, String> {
        let (username, password) = self.credentials(wizard)?;
        let url = format!("{}/auth/connect", self.base_url);
        let response = waki::Client::new().post(&url)
            .connect_timeout(self.timeout)
            .header("Accept", "application/json")
            .form([("player", username), ("password", password)])
            .send().map_err(|e| unreachable_error(&self.base_url, &e))?;
        let status = response.status_code();
        let token = response.header("x-moor-auth-token")
            .and_then(|h| h.to_str().ok()).map(str::to_string);
        let body = response.body().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("login as {username:?} failed with HTTP {status}: {}", clip(&String::from_utf8_lossy(&body), 300)));
        }
        let token = token.ok_or("login succeeded but X-Moor-Auth-Token was absent")?;
        sys::kv_put("global", &self.cache_key(wizard)?, &token);
        Ok(token)
    }

    pub fn authenticated(&self, method: &str, path: &str, content_type: Option<&str>, body: Option<&str>, wizard: bool) -> Result<HttpResponse, String> {
        let token = self.token(wizard)?;
        let first = self.request_with(method, path, Some("application/json"), content_type, body, Some(&token))?;
        if first.status != 401 { return Ok(first); }
        self.clear_cached_token(wizard)?;
        let token = self.login(wizard)?;
        self.request_with(method, path, Some("application/json"), content_type, body, Some(&token))
    }

    pub fn captured(&self, path: &str, source: &str, timeout_ms: Option<u64>, wizard: bool) -> Result<CapturedInvocation, String> {
        let ms = timeout_ms.unwrap_or(0);
        if ms > 300_000 { return Err("timeout_ms exceeds mooR's 300000 ms protocol ceiling".into()); }
        let path = if ms == 0 { path.to_string() } else { format!("{path}?timeout_ms={ms}") };
        let response = self.authenticated("POST", &path, Some("text/plain; charset=utf-8"), Some(source), wizard)?;
        decode_captured(&response, &path)
    }

    fn request_with(&self, method: &str, path: &str, accept: Option<&str>, content_type: Option<&str>, body: Option<&str>, token: Option<&str>) -> Result<HttpResponse, String> {
        let url = format!("{}{}", self.base_url, path);
        sys::log(LogLevel::Debug, &format!("moo: {method} {path}"));
        let client = waki::Client::new();
        let mut request = match method { "GET" => client.get(&url), "POST" => client.post(&url), other => return Err(format!("unsupported HTTP method {other:?}")) };
        request = request.connect_timeout(self.request_timeout);
        if let Some(v) = accept { request = request.header("Accept", v); }
        if let Some(v) = content_type { request = request.header("Content-Type", v); }
        if let Some(v) = token { request = request.header("X-Moor-Auth-Token", v); }
        if let Some(v) = body { request = request.body(v); }
        let response = request.send().map_err(|e| unreachable_error(&self.base_url, &e))?;
        let status = response.status_code();
        let content_type = response.header("content-type").and_then(|h| h.to_str().ok().map(str::to_string));
        let body = response.body().map_err(|e| format!("could not read the response body from {url}: {e}"))?;
        Ok(HttpResponse { status, content_type, body })
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        accept: Option<&str>,
    ) -> Result<HttpResponse, String> {
        let url = format!("{}{}", self.base_url, path);
        sys::log(LogLevel::Debug, &format!("moo: {method} {path}"));

        let client = waki::Client::new();
        let mut request = match method {
            "GET" => client.get(&url),
            other => return Err(format!("unsupported HTTP method {other:?}")),
        };

        request = request.connect_timeout(self.timeout);
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }
        if let Some(token) = &self.auth_token {
            request = request.header("X-Moor-Auth-Token", token);
        }

        let response = request.send().map_err(|e| unreachable_error(&self.base_url, &e))?;

        let status = response.status_code();
        let content_type = response
            .header("content-type")
            .and_then(|h| h.to_str().ok().map(str::to_string));
        let bytes = response
            .body()
            .map_err(|e| format!("could not read the response body from {url}: {e}"))?;

        Ok(HttpResponse {
            status,
            content_type,
            body: bytes,
        })
    }
}

/// A raw HTTP response, kept undecoded so a caller can pick apart status,
/// content type and body independently — `/health` and `/version` both use
/// this without any FlatBuffers unwrapping.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CapturedInvocation {
    pub success: bool,
    pub value: Option<Value>,
    pub error: Option<Value>,
    pub output: Vec<Value>,
    pub timed_out: bool,
    pub cancelled: bool,
}

pub fn decode_captured(response: &HttpResponse, path: &str) -> Result<CapturedInvocation, String> {
    if !response.ok() { return Err(explain_status(response, path)); }
    let body = response.json()?;

    // Current web hosts return a bare InvocationResponse containing an
    // InvocationSuccess/InvocationError outcome. Older deployed hosts wrap a
    // successful captured eval/command as ReplyResult -> ClientSuccess ->
    // EvalResult. Accept both wire shapes while preserving genuine errors.
    let explicit = find_key(&body, &["InvocationSuccess", "InvocationError", "VerbCallSuccess", "VerbCallError"]);
    let legacy_success = find_named(&body, "EvalResult")
        .and_then(|node| find_named(node, "result"));
    let legacy_error = find_key(&body, &["ClientFailure", "TaskError", "SchedulerError"]);

    let (success, node) = if let Some((kind, node)) = explicit {
        (kind == "InvocationSuccess" || kind == "VerbCallSuccess", node)
    } else if let Some(node) = legacy_success {
        (true, node)
    } else if let Some((_, node)) = legacy_error {
        (false, node)
    } else {
        return Err(format!(
            "{path} returned an unrecognised captured-invocation envelope: {}",
            clip(&body.to_string(), 1200)
        ));
    };

    let output = find_named(&body, "output")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(decode_wire_value).collect())
        .unwrap_or_default();
    let text = node.to_string().to_ascii_lowercase();
    let value = if success {
        let raw = find_named(node, "result").unwrap_or(node);
        Some(decode_wire_value(raw))
    } else {
        None
    };
    Ok(CapturedInvocation {
        success,
        value,
        error: if success { None } else { Some(decode_wire_value(node)) },
        output,
        timed_out: text.contains("taskabortedlimit") || text.contains("timeout") || text.contains("time limit"),
        cancelled: text.contains("cancel"),
    })
}

/// Convert mooR's tagged JSON representation of a Var into ordinary JSON.
/// Unknown tags are retained rather than discarded so protocol additions are
/// visible to callers instead of becoming null.
fn decode_wire_value(value: &Value) -> Value {
    let Some(variant) = value.get("variant").and_then(Value::as_object) else {
        return match value {
            Value::Array(items) => Value::Array(items.iter().map(decode_wire_value).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), decode_wire_value(v))).collect()),
            _ => value.clone(),
        };
    };
    let Some((tag, payload)) = variant.iter().next() else { return value.clone(); };
    match tag.as_str() {
        "VarNone" => Value::Null,
        "VarBool" => payload.get("value").cloned().unwrap_or(Value::Bool(false)),
        "VarInt" | "VarFloat" | "VarStr" => payload.get("value").cloned().unwrap_or(Value::Null),
        "VarObj" => decode_wire_object(payload).unwrap_or_else(|| value.clone()),
        "VarList" => payload.get("elements").and_then(Value::as_array)
            .map(|a| Value::Array(a.iter().map(decode_wire_value).collect()))
            .unwrap_or_else(|| value.clone()),
        "VarMap" => {
            let Some(pairs) = payload.get("pairs").and_then(Value::as_array) else { return value.clone(); };
            let decoded: Vec<(Value, Value)> = pairs.iter().filter_map(|pair| {
                Some((decode_wire_value(pair.get("key")?), decode_wire_value(pair.get("value")?)))
            }).collect();
            if decoded.iter().all(|(k, _)| k.is_string()) {
                Value::Object(decoded.into_iter().map(|(k, v)| (k.as_str().unwrap().to_string(), v)).collect())
            } else {
                Value::Array(decoded.into_iter().map(|(k, v)| json!({"key": k, "value": v})).collect())
            }
        }
        _ => value.clone(),
    }
}

fn decode_wire_object(payload: &Value) -> Option<Value> {
    let obj = find_named(payload, "ObjId")?;
    let id = obj.get("id")?;
    Some(Value::String(format!("#{}", id)))
}

fn find_named<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(name).or_else(|| m.values().find_map(|v| find_named(v, name))),
        Value::Array(a) => a.iter().find_map(|v| find_named(v, name)),
        _ => None,
    }
}

fn find_key<'a, 'b>(v: &'a Value, names: &'b [&'b str]) -> Option<(&'b str, &'a Value)> {
    match v {
        Value::Object(m) => {
            for name in names { if let Some(v) = m.get(*name) { return Some((*name, v)); } }
            m.values().find_map(|v| find_key(v, names))
        }
        Value::Array(a) => a.iter().find_map(|v| find_key(v, names)),
        _ => None,
    }
}

pub fn path_segment(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':') { out.push(b as char); }
        else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

/// Normalize the human-facing object notation accepted by Thetis tools to the
/// CURIE notation required by mooR's HTTP routes. MOO expressions still use
/// `#123`; only REST path/query parameters need `oid:123`.
pub fn object_path_segment(value: &str) -> Result<String, String> {
    let value = value.trim();
    if let Some(id) = value.strip_prefix('#') {
        // A UUID object id, as `Display for Obj` and `compact_wire` render one:
        // `#RRRRRR-TTTTTTTTTT` in hex. Its CURIE is `uuid:` plus the same text,
        // per `ObjectRef::parse_curie`. Checked before the numeric case because
        // a plain `#-5` and a UUID both contain a dash, and without this a
        // caller could not pass an id back that a listing had just shown it.
        if let Some((first, rest)) = id.split_once('-') {
            let hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
            if first.len() == 6 && rest.len() == 10 && hex(first) && hex(rest) {
                return Ok(path_segment(&format!("uuid:{id}")));
            }
        }
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Ok(path_segment(&format!("oid:{id}")));
        }
        return Err(format!(
            "invalid object reference {value:?}: expected #number, a UUID id such as \
             #0011E5-9CB7359F34, or a mooR CURIE such as oid:36 or sysobj:system"
        ));
    }
    if value.starts_with("oid:") || value.starts_with("uuid:") || value.starts_with("sysobj:") || value.starts_with("match(\"") {
        return Ok(path_segment(value));
    }
    Err("object must be #number or a mooR CURIE such as oid:36, uuid:..., or sysobj:system".into())
}

pub fn dynamic_tools(value: &Value) -> Result<Vec<Value>, String> {
    fn find(v: &Value) -> Option<&Vec<Value>> {
        match v {
            Value::Array(a) if a.iter().all(|x| x.as_object().is_some()) => Some(a),
            Value::Array(a) => a.iter().find_map(find),
            Value::Object(m) => m.values().find_map(find),
            _ => None,
        }
    }
    let tools = find(value).ok_or("external_agent_tools did not return a list of maps")?;
    let mut valid = Vec::new();
    for tool in tools {
        let m = tool.as_object().unwrap();
        for key in ["name", "description", "target_obj", "target_verb", "input_schema"] {
            if !m.contains_key(key) { return Err(format!("dynamic tool missing {key}")); }
        }
        if !m["name"].is_string() || !m["description"].is_string() || !m["target_verb"].is_string() || !m["input_schema"].is_object() {
            return Err("dynamic tool has invalid field types".into());
        }
        valid.push(tool.clone());
    }
    Ok(valid)
}

pub fn objdef_lines_literal(text: &str) -> Result<String, String> {
    let lines = text.lines().map(|line| moo_literal(&Value::String(line.to_string()))).collect::<Result<Vec<_>, _>>()?;
    Ok(format!("{{{}}}", lines.join(", ")))
}

pub fn objdef_constants_builder() -> &'static str {
    "constants = []; for o in (objects()) id = object_metadata(o, 'import_export_id); if (typeof(id) == TYPE_STR && id != \"\") constants[id:uppercase()] = o; endif endfor"
}

pub fn confined_objdef_path(path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, Path, PathBuf};
    let path = Path::new(path);
    if path.is_absolute() { return Err("objdef path must be relative to workspace/torchship-objdef".into()); }
    for part in path.components() {
        if !matches!(part, Component::Normal(_)) { return Err("objdef path may not contain '.', '..', roots, or prefixes".into()); }
    }
    let root = PathBuf::from("workspace/torchship-objdef");
    let full = root.join(path);
    if full.exists() {
        let canonical_root = std::fs::canonicalize(&root).map_err(|e| format!("cannot resolve objdef root: {e}"))?;
        let canonical = std::fs::canonicalize(&full).map_err(|e| format!("cannot resolve objdef path: {e}"))?;
        if !canonical.starts_with(canonical_root) { return Err("objdef path escapes confined root".into()); }
    } else if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create objdef directory: {e}"))?;
        let canonical_root = std::fs::canonicalize(&root).map_err(|e| format!("cannot resolve objdef root: {e}"))?;
        let canonical_parent = std::fs::canonicalize(parent).map_err(|e| format!("cannot resolve objdef parent: {e}"))?;
        if !canonical_parent.starts_with(canonical_root) { return Err("objdef path escapes confined root".into()); }
    }
    Ok(full)
}

pub fn moo_environment_literal(value: &Value) -> Result<String, String> {
    let entries = value.as_array().ok_or("environment must be an array")?;
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            Value::String(s) => out.push(moo_object_expr(s)?),
            Value::Number(n) => out.push(moo_object_expr(&format!("#{}", n))?),
            Value::Object(map) => {
                let obj = map.get("obj").ok_or("environment entry missing obj")?;
                let obj = match obj { Value::String(s) => moo_object_expr(s)?, Value::Number(n) => moo_object_expr(&format!("#{}", n))?, _ => return Err("environment obj must be an object reference".into()) };
                if let Some(names) = map.get("names") {
                    let names = names.as_array().ok_or("environment names must be an array")?;
                    let names = names.iter().map(moo_literal).collect::<Result<Vec<_>, _>>()?;
                    out.push(format!("{{{}, {{{}}}}}", obj, names.join(", ")));
                } else { out.push(obj); }
            }
            _ => return Err("invalid environment entry".into()),
        }
    }
    Ok(format!("{{{}}}", out.join(", ")))
}

pub fn moo_object_expr(value: &str) -> Result<String, String> {
    let value = value.trim();
    if let Some(id) = value.strip_prefix('#') {
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Ok(value.to_string());
        }
    }
    if value.starts_with("moor:") || value.starts_with("uuid:") {
        return Ok(format!("toobj({})", moo_literal(&Value::String(value.to_string()))?));
    }
    Err("object must be a #number or mooR CURIE such as moor:system".into())
}

pub fn moo_literal(value: &Value) -> Result<String, String> {
    match value {
        Value::Null => Ok("0".into()),
        Value::Bool(v) => Ok(if *v { "true" } else { "false" }.into()),
        Value::Number(v) => Ok(v.to_string()),
        Value::String(v) => Ok(format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t"))),
        Value::Array(values) => Ok(format!("{{{}}}", values.iter().map(moo_literal).collect::<Result<Vec<_>, _>>()?.join(", "))),
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(v) = map.get("$object").and_then(Value::as_str) {
                    if v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_digit() || c == '-') { return Ok(v.into()); }
                    return Err("$object must be a numeric object reference such as #123".into());
                }
                if let Some(v) = map.get("$symbol").and_then(Value::as_str) {
                    if !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { return Ok(format!("'{v}")); }
                    return Err("$symbol contains unsafe characters".into());
                }
                if let Some(v) = map.get("$error").and_then(Value::as_str) {
                    if v.starts_with("E_") && v.chars().all(|c| c.is_ascii_uppercase() || c == '_') { return Ok(v.into()); }
                    return Err("$error must be a builtin error name such as E_PERM".into());
                }
            }
            let pairs = map.iter().map(|(k, v)| Ok(format!("{} -> {}", moo_literal(&Value::String(k.clone()))?, moo_literal(v)?))).collect::<Result<Vec<String>, String>>()?;
            Ok(format!("[{}]", pairs.join(", ")))
        }
    }
}

/// Strips the FlatBuffers union wrappers out of a decoded wire response.
///
/// The web host serializes the same tagged enums it uses for the ZeroMQ RPC
/// protocol, so a plain `to_string_pretty` of a verb list spends five levels of
/// nesting on every integer:
///
/// ```text
/// {"names":[{"value":"aliases"}],"location":{"obj":{"ObjId":{"id":1}}}}
/// {"names":["aliases"],"location":"#1"}
/// ```
///
/// `#1`'s verb list measured 40,968 bytes before this and 6,090 after — the same
/// information, because nothing is summarised or dropped. This is worth doing
/// before any cap: a cut is a loss, whereas deleting a wrapper that means
/// nothing is free, and it is what keeps these results under the cap at all.
///
/// The rule that makes it safe: this protocol's union tags are CamelCase
/// (`ClientSuccess`, `VerbsReply`, `ObjId`, `VarInt`) while its real field names
/// are snake_case (`arg_spec`, `names`, `owner`). So a single-key object is a
/// wrapper — and can be unwrapped — exactly when its key is CamelCase or one of
/// the known plumbing names. Anything else is kept, including unknown tags, so a
/// protocol addition shows up rather than vanishing.
pub fn compact_wire(value: &Value) -> Value {
    /// A chain of single-key wrappers ending in an object id, rendered the way
    /// mooR itself renders one.
    ///
    /// Two forms reach us, because a server with `use_uuobjids` on mixes them
    /// in a single list:
    ///
    /// * `ObjId{id}` — a plain numbered object, `#36`.
    /// * `UuObjId{packed_value}` — a time-ordered id whose `u64` packs an
    ///   autoincrement, six bits of randomness and a millisecond timestamp.
    ///   `#{first_group:06X}-{epoch_ms:010X}`, matching `UuObjid::to_uuid_string`
    ///   and `Display for Obj` in `crates/var/src/obj.rs`. Leaving it as the raw
    ///   integer would put a number in front of a caller that no other tool
    ///   accepts back.
    fn as_objid(v: &Value) -> Option<String> {
        let map = v.as_object()?;
        if map.len() != 1 {
            return None;
        }
        let (key, inner) = map.iter().next()?;
        match key.as_str() {
            "ObjId" => {
                let fields = inner.as_object()?;
                if fields.len() == 1 {
                    return Some(format!("#{}", fields.get("id")?.as_u64()?));
                }
                None
            }
            "UuObjId" => {
                let fields = inner.as_object()?;
                if fields.len() != 1 {
                    return None;
                }
                let packed = fields.get("packed_value")?.as_u64()?;
                // The same field layout as schema/src/packed_id.rs.
                let autoincrement = (packed >> 46) & 0xFFFF;
                let rng = (packed >> 40) & 0x3F;
                let epoch_ms = packed & 0x00FF_FFFF_FFFF;
                let first_group = (autoincrement << 6) | rng;
                Some(format!("#{first_group:06X}-{epoch_ms:010X}"))
            }
            _ => as_objid(inner),
        }
    }

    /// Whether a single-key object's key carries no information.
    fn is_wrapper(key: &str) -> bool {
        matches!(key, "value" | "reply" | "result" | "obj" | "variant")
            || key.starts_with(|c: char| c.is_ascii_uppercase())
    }

    if let Some(id) = as_objid(value) {
        return Value::String(id);
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(compact_wire).collect()),
        Value::Object(map) => {
            if map.len() == 1 {
                let (key, inner) = map.iter().next().expect("len checked");
                if is_wrapper(key) {
                    return compact_wire(inner);
                }
            }
            Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), compact_wire(v)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

/// Caps a result, keeping the head, and says how to reach the rest.
///
/// The old version stopped at `...[truncated: N bytes total]`, which tells the
/// caller its answer is incomplete and gives it nothing to do about that. The
/// only recoveries available are re-asking for a narrower slice or reading the
/// spilled copy the host leaves in the workspace, so the note names both. Cutting
/// on a line boundary matters here because objdef and pretty JSON are read by
/// line, and half a line reads as a syntax error rather than as a cut.
pub fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes { return text.into(); }
    // The note costs about 240 bytes. Subtracting it from a small budget can
    // leave nothing at all, and a note with no excerpt attached is the one
    // outcome with no value to anyone: the caller learns neither the content
    // nor anything it did not already know. Keep a floor of real text and let
    // the result run slightly over instead — the host's cap is the real
    // backstop, and it spills rather than cutting.
    let mut end = max_bytes.saturating_sub(240).max(max_bytes / 2).min(text.len());
    while !text.is_char_boundary(end) { end -= 1; }
    // Prefer the last line break in the final 10%, so the excerpt ends on a
    // whole line where the text has any.
    if let Some(nl) = text[..end].rfind('\n') {
        if nl > end.saturating_sub(end / 10) { end = nl; }
    }
    let shown_lines = text[..end].lines().count();
    let total_lines = text.lines().count();
    format!(
        "{}\n\n...[cut here: showed {} of {} bytes, lines 1-{} of {}. \
Ask for a narrower slice — a single verb or property rather than a whole object, \
or this tool's offset/limit where it has them. The complete output is also spilled \
to a file under /workspace/tool-output/, which read_path and search_files can window \
and grep.]",
        &text[..end],
        end,
        text.len(),
        shown_lines,
        total_lines,
    )
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// Parses the body as JSON, with an error that names what actually came
    /// back rather than a bare serde message — a FlatBuffers binary blob
    /// parsed as JSON produces "expected value at line 1 column 1", which
    /// tells nobody anything.
    pub fn json(&self) -> Result<Value, String> {
        if self.body.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&self.text()).map_err(|e| {
            let ct = self.content_type.as_deref().unwrap_or("unknown");
            format!(
                "the response was not JSON (Content-Type: {ct}): {e}. If the server sent \
                 FlatBuffers, request it with an explicit Accept: application/json header."
            )
        })
    }
}

fn unreachable_error(base_url: &str, e: &impl std::fmt::Display) -> String {
    format!(
        "could not reach the mooR web host at {base_url}: {e}\n\n\
         Check that a server is actually listening there and that [tools.moo] base_url in \
         thetis.toml points at the right host and port. The expected live deployment answers on \
         {DEFAULT_BASE_URL}."
    )
}

// ---------------------------------------------------------------------------
// Endpoint-specific helpers
// ---------------------------------------------------------------------------

/// The result of a health check: reachable and healthy, reachable but
/// unhealthy, or unreachable. Kept as a type rather than a bare bool so a
/// caller can render "the web host answered but says it is unhealthy"
/// distinctly from "nothing answered at all".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// `/health` returned 200: the web host has heard from its daemon
    /// recently (within the last 30 seconds, or not at all yet since
    /// startup — the daemon-side check treats a fresh host as healthy).
    Healthy,
    /// `/health` returned 503: the web host is up but has not heard from its
    /// daemon recently. The HTTP layer answers; the world behind it may not.
    Unhealthy,
}

/// Calls `/health` and classifies the result. Any status other than 200/503
/// is reported as an error string rather than guessed at, since the mooR web
/// host source only ever returns those two.
pub fn health(client: &Moo) -> Result<Health, String> {
    let response = client.get_plain("/health")?;
    match response.status {
        200 => Ok(Health::Healthy),
        503 => Ok(Health::Unhealthy),
        other => Err(format!(
            "/health returned an unexpected status {other}, expected 200 or 503: {}",
            clip(&response.text(), 300)
        )),
    }
}

/// Server version and commit, from `GET /version`.
pub struct Version {
    pub version: String,
    pub commit: String,
}

pub fn version(client: &Moo) -> Result<Version, String> {
    let response = client.get_json("/version")?;
    if !response.ok() {
        return Err(explain_status(&response, "/version"));
    }
    let body = response.json()?;
    Ok(Version {
        version: body
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        commit: body
            .get("commit")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    })
}

/// Fetches `/v1/features` as JSON and unwraps mooR's nested wire-envelope
/// down to the flat feature map, e.g.
/// `{"persistent_tasks": true, "rich_notify": true, ...}`.
///
/// The nesting (`result.HostSuccess.reply.reply.ServerFeatures`) is the same
/// tagged-union shape the daemon uses over ZeroMQ, serialized as-is by the
/// web host rather than flattened for HTTP callers — confirmed against a
/// live server while writing this client. A server that changes that nesting
/// is a live protocol change, not a bug here, so this walks down looking for
/// the first object that looks like a feature map (all-boolean values)
/// instead of hard-coding the exact path, and returns the raw body
/// unmodified if that search fails — degrade, do not fail outright, per
/// `moor/services/clients-and-web-ui`'s rule for an evolving reply shape.
pub fn features(client: &Moo) -> Result<Value, String> {
    let response = client.get_json("/v1/features")?;
    if !response.ok() {
        return Err(explain_status(&response, "/v1/features"));
    }
    let body = response.json()?;
    Ok(find_feature_map(&body).cloned().unwrap_or(body))
}

/// Depth-first search for the first JSON object whose values are all
/// booleans and which has at least one entry — that is what a feature-flag
/// table looks like, however many union layers it is wrapped in.
fn find_feature_map(value: &Value) -> Option<&Value> {
    if let Value::Object(map) = value {
        if !map.is_empty() && map.values().all(|v| v.is_boolean()) {
            return Some(value);
        }
        for v in map.values() {
            if let Some(found) = find_feature_map(v) {
                return Some(found);
            }
        }
    }
    None
}

fn explain_status(response: &HttpResponse, path: &str) -> String {
    match response.status {
        401 => format!(
            "{path} returned 401 Unauthorized. This tool does not send credentials for this \
             call; if the server now requires them, that is a change from what \
             crates/web-host currently does."
        ),
        403 => format!("{path} returned 403 Forbidden: {}", clip(&response.text(), 300)),
        404 => format!(
            "{path} returned 404 Not Found. The server at this base_url may not be a mooR web \
             host, or this route does not exist on its version."
        ),
        406 => format!(
            "{path} returned 406 Not Acceptable. The server could not satisfy \
             Accept: application/json, which is unexpected — mooR's web host is supposed to \
             support both application/json and application/x-flatbuffers on this route."
        ),
        503 => format!(
            "{path} returned 503 Service Unavailable. The web host is up but its daemon \
             connection is not: this usually means the same thing as an unhealthy /health check."
        ),
        other => format!(
            "{path} returned {other}: {}",
            clip(&response.text(), 300)
        ),
    }
}

// ---------------------------------------------------------------------------
// Small formatting helpers
// ---------------------------------------------------------------------------

/// Truncates on a character boundary, never slicing a multi-byte char in
/// half.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Config resolution
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Wire compaction
    // -----------------------------------------------------------------------

    /// One verb, exactly as `GET /v1/verbs/oid:1?inherited=true` returns it on
    /// mooR 2.0.0-dev. Copied from a live response rather than invented, because
    /// the whole point of `compact_wire` is knowing which keys are plumbing.
    #[test]
    fn compaction_strips_the_union_wrappers_and_keeps_every_field() {
        let live = json!({
            "result": { "ClientSuccess": { "reply": { "reply": { "VerbsReply": { "verbs": [{
                "arg_spec": [{ "value": "this" }, { "value": "none" }, { "value": "this" }],
                "d": true,
                "location": { "obj": { "ObjId": { "id": 1 } } },
                "names": [{ "value": "aliases" }],
                "owner": { "obj": { "ObjId": { "id": 36 } } },
                "r": true, "w": false, "x": true
            }] } } } } }
        });

        assert_eq!(
            compact_wire(&live),
            json!({ "verbs": [{
                "arg_spec": ["this", "none", "this"],
                "d": true,
                "location": "#1",
                "names": ["aliases"],
                "owner": "#36",
                "r": true, "w": false, "x": true
            }] }),
            "compaction should drop only the wrappers"
        );
    }

    /// A whole response can be a single value buried in wrappers: `GET
    /// /v1/objects/oid:1` is 459 pretty bytes that mean `#1`.
    #[test]
    fn a_response_that_is_only_wrappers_compacts_to_its_value() {
        let live = json!({
            "result": { "ClientSuccess": { "reply": { "reply": { "ResolveResult": {
                "result": { "variant": { "VarObj": { "obj": { "obj": { "ObjId": { "id": 1 } } } } } }
            } } } } }
        });
        assert_eq!(compact_wire(&live), json!("#1"));
    }

    /// Real field names must survive even when they look structural, and an
    /// unknown CamelCase tag must not take its payload with it silently.
    #[test]
    fn informative_keys_are_never_unwrapped() {
        // A single-key object whose key is snake_case is data, not a wrapper.
        let data = json!({ "verbs": [1, 2] });
        assert_eq!(compact_wire(&data), data);

        // Multi-key objects are never unwrapped, whatever the keys are called.
        let multi = json!({ "value": 1, "other": 2 });
        assert_eq!(compact_wire(&multi), multi);

        // An ObjId with extra fields is not the shape we know, so it is kept
        // rather than guessed at.
        let odd = json!({ "ObjId": { "id": 1, "generation": 7 } });
        assert_eq!(compact_wire(&odd), json!({ "id": 1, "generation": 7 }));
    }

    /// An id this family prints must be one this family accepts back. A
    /// listing that shows `#0011E5-9CB7359F34` is useless if no tool takes it.
    #[test]
    fn a_rendered_uuid_id_round_trips_as_an_object_reference() {
        assert_eq!(
            object_path_segment("#0011E5-9CB7359F34").unwrap(),
            "uuid:0011E5-9CB7359F34"
        );
        // Plain numbered objects, including negatives, keep the oid: form.
        assert_eq!(object_path_segment("#36").unwrap(), "oid:36");
        assert_eq!(object_path_segment("#-1").unwrap(), "oid:-1");
        // Not the UUID shape: wrong group lengths, or non-hex.
        for bad in ["#11E5-9CB7359F34", "#0011E5-9CB7359F3", "#00ZZE5-9CB7359F34"] {
            assert!(
                object_path_segment(bad).is_err(),
                "{bad} should not parse as an object reference"
            );
        }
    }

    /// A UUID object id must render as mooR renders it, not as the packed
    /// integer — a caller cannot pass `5037535855484724` to any other tool.
    ///
    /// The expected strings come from a live `/v1/objects/query?parent=oid:1`
    /// on a server with `use_uuobjids` enabled.
    #[test]
    fn uuid_object_ids_are_rendered_the_way_moor_renders_them() {
        for (packed, expected) in [
            (5037535855484724u64, "#0011E5-9CB7359F34"),
            (253123655460440414u64, "#038346-9F9FF7C95E"),
            (105895750294515354u64, "#017837-9FB3872E9A"),
        ] {
            let live = json!({ "obj": { "UuObjId": { "packed_value": packed } } });
            assert_eq!(compact_wire(&live), json!(expected), "packed {packed}");
        }
    }

    #[test]
    fn base_url_defaults_when_config_has_nothing() {
        let client = Moo::from_value(&json!({})).unwrap();
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
        assert!(client.auth_token.is_none());
    }

    #[test]
    fn base_url_is_read_from_config_and_trailing_slash_is_trimmed() {
        let client = Moo::from_value(&json!({ "base_url": "http://example.com:9999/" })).unwrap();
        assert_eq!(client.base_url, "http://example.com:9999");
    }

    #[test]
    fn url_is_accepted_as_an_alias_for_base_url() {
        let client = Moo::from_value(&json!({ "url": "https://example.org" })).unwrap();
        assert_eq!(client.base_url, "https://example.org");
    }

    #[test]
    fn base_url_wins_over_url_when_both_are_set() {
        let client = Moo::from_value(&json!({
            "base_url": "http://a.example",
            "url": "http://b.example",
        }))
        .unwrap();
        assert_eq!(client.base_url, "http://a.example");
    }

    #[test]
    fn blank_base_url_falls_back_to_the_default_rather_than_erroring() {
        let client = Moo::from_value(&json!({ "base_url": "   " })).unwrap();
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn a_base_url_with_no_scheme_is_rejected() {
        let err = match Moo::from_value(&json!({ "base_url": "10.10.10.1:7892" })) {
            Ok(_) => panic!("expected a scheme-less base_url to be rejected"),
            Err(e) => e,
        };
        assert!(err.contains("http://") || err.contains("https://"), "{err}");
    }

    #[test]
    fn auth_token_is_read_when_present() {
        let client = Moo::from_value(&json!({ "auth_token": "tok123" })).unwrap();
        assert_eq!(client.auth_token, Some("tok123".to_string()));
    }

    #[test]
    fn token_is_accepted_as_an_alias_for_auth_token() {
        let client = Moo::from_value(&json!({ "token": "tok456" })).unwrap();
        assert_eq!(client.auth_token, Some("tok456".to_string()));
    }

    #[test]
    fn timeout_secs_is_clamped_to_a_sane_range() {
        let too_small = Moo::from_value(&json!({ "timeout_secs": 0 })).unwrap();
        assert_eq!(too_small.timeout, Duration::from_secs(1));

        let too_big = Moo::from_value(&json!({ "timeout_secs": 99_999 })).unwrap();
        assert_eq!(too_big.timeout, Duration::from_secs(120));

        let ordinary = Moo::from_value(&json!({ "timeout_secs": 30 })).unwrap();
        assert_eq!(ordinary.timeout, Duration::from_secs(30));
    }

    // -----------------------------------------------------------------------
    // HttpResponse helpers
    // -----------------------------------------------------------------------

    #[test]
    fn ok_is_true_only_for_2xx() {
        let ok = HttpResponse { status: 200, content_type: None, body: vec![] };
        let redirect = HttpResponse { status: 302, content_type: None, body: vec![] };
        let client_err = HttpResponse { status: 404, content_type: None, body: vec![] };
        assert!(ok.ok());
        assert!(!redirect.ok());
        assert!(!client_err.ok());
    }

    #[test]
    fn empty_body_parses_as_an_empty_json_object() {
        let response = HttpResponse { status: 200, content_type: None, body: vec![] };
        assert_eq!(response.json().unwrap(), json!({}));
    }

    #[test]
    fn json_parse_failure_names_the_content_type() {
        // A FlatBuffers binary blob is not valid UTF-8 in general, but this
        // stands in for "whatever came back was not JSON" regardless of why.
        let response = HttpResponse {
            status: 200,
            content_type: Some("application/x-flatbuffers".to_string()),
            body: b"\x00\x01\x02not json".to_vec(),
        };
        let err = response.json().unwrap_err();
        assert!(err.contains("x-flatbuffers"), "{err}");
        assert!(err.contains("Accept: application/json"), "{err}");
    }

    #[test]
    fn valid_json_body_parses() {
        let response = HttpResponse {
            status: 200,
            content_type: Some("application/json".to_string()),
            body: br#"{"version":"2.0.0-dev","commit":"abc123"}"#.to_vec(),
        };
        let body = response.json().unwrap();
        assert_eq!(body["version"], "2.0.0-dev");
    }

    // -----------------------------------------------------------------------
    // Feature-map unwrapping
    // -----------------------------------------------------------------------

    #[test]
    fn feature_map_is_found_under_the_real_nesting_seen_from_a_live_server() {
        // Exact shape returned by a live mooR 2.0.0-dev web host for
        // GET /v1/features with Accept: application/json.
        let body = json!({
            "result": {
                "HostSuccess": {
                    "reply": {
                        "reply": {
                            "ServerFeatures": {
                                "persistent_tasks": true,
                                "rich_notify": true,
                                "lexical_scopes": true,
                                "type_dispatch": true,
                                "flyweight_type": true,
                                "list_comprehensions": true,
                                "bool_type": true,
                                "use_boolean_returns": false,
                                "symbol_type": false,
                                "use_symbols_in_builtins": false,
                                "custom_errors": true,
                                "use_uuobjids": true,
                                "enable_eventlog": false,
                                "anonymous_objects": false
                            }
                        }
                    }
                }
            }
        });

        let found = find_feature_map(&body).expect("a feature map should be found");
        assert_eq!(found["persistent_tasks"], true);
        assert_eq!(found["enable_eventlog"], false);
        assert_eq!(found.as_object().unwrap().len(), 14);
    }

    #[test]
    fn feature_map_search_returns_none_on_a_shape_with_no_all_boolean_object() {
        let body = json!({ "result": { "some_count": 3, "name": "x" } });
        assert!(find_feature_map(&body).is_none());
    }

    #[test]
    fn feature_map_search_ignores_an_empty_object() {
        // An empty object technically satisfies "all values are booleans"
        // vacuously; it must not be mistaken for a feature map.
        let body = json!({ "wrapper": {} });
        assert!(find_feature_map(&body).is_none());
    }

    #[test]
    fn features_falls_back_to_the_raw_body_when_no_feature_map_is_found() {
        // find_feature_map returning None must not become an error: a future
        // server reshaping this reply should degrade, not break the tool.
        let body = json!({ "totally": "different", "shape": 1 });
        assert_eq!(find_feature_map(&body), None);
    }

    // -----------------------------------------------------------------------
    // Status explanations
    // -----------------------------------------------------------------------

    #[test]
    fn explain_status_names_the_path_for_every_status_seen_live() {
        for status in [401, 403, 404, 406, 503, 500] {
            let response = HttpResponse { status, content_type: None, body: vec![] };
            let msg = explain_status(&response, "/v1/features");
            assert!(msg.contains("/v1/features"), "{msg}");
            assert!(msg.contains(&status.to_string()), "{msg}");
        }
    }

    // -----------------------------------------------------------------------
    // clip
    // -----------------------------------------------------------------------

    #[test]
    fn literals_escape_injection_and_support_tagged_values() {
        assert_eq!(moo_literal(&json!("x\"); recycle(#1); \"")).unwrap(), "\"x\\\"); recycle(#1); \\\"\"");
        assert_eq!(moo_literal(&json!({"$object":"#42"})).unwrap(), "#42");
        assert_eq!(moo_literal(&json!({"$symbol":"alpha_2"})).unwrap(), "'alpha_2");
        assert_eq!(moo_literal(&json!({"$error":"E_PERM"})).unwrap(), "E_PERM");
        assert!(moo_literal(&json!({"$object":"#1); recycle(#2)"})).is_err());
        assert_eq!(moo_literal(&json!({"a":[1,true]})).unwrap(), "[\"a\" -> {1, true}]");
    }

    #[test]
    fn paths_are_percent_encoded() {
        assert_eq!(path_segment("moor:1"), "moor:1");
        assert_eq!(path_segment("verb name/雪"), "verb%20name%2F%E9%9B%AA");
    }

    #[test]
    fn captured_error_keeps_output_and_classifies_timeout() {
        let response = HttpResponse { status: 200, content_type: Some("application/json".into()), body: serde_json::to_vec(&json!({
            "outcome": {"InvocationError": {"error": {"TaskAbortedLimit":"time"}}},
            "output": [{"NotifyEvent":{"value":"already committed"}}]
        })).unwrap() };
        let decoded = decode_captured(&response, "/v1/eval").unwrap();
        assert!(!decoded.success);
        assert!(decoded.timed_out);
        assert_eq!(decoded.output.len(), 1);
    }

    #[test]
    fn config_requires_credential_pairs() {
        assert!(Moo::from_value(&json!({"username":"only"})).is_err());
        assert!(Moo::from_value(&json!({"wizard_password":"only"})).is_err());
    }

    #[test]
    fn bounded_output_is_utf8_safe() {
        let value = "雪".repeat(200);
        let clipped = bounded(&value, 128);
        assert!(clipped.len() < value.len());
        // Multi-byte characters must not be cut in half: the excerpt is still
        // valid UTF-8 by construction, so check the boundary held by counting.
        assert!(clipped.starts_with('雪'));
    }

    /// A cap that says only "this is incomplete" leaves the caller nothing to
    /// do but guess or re-run the same call, so the note has to name a way on.
    /// This asserts the contract rather than the old `truncated` wording.
    #[test]
    fn a_bounded_result_says_how_to_get_the_rest() {
        let clipped = bounded(&"x\n".repeat(40_000), 4_000);
        for expected in ["cut here", "narrower slice", "/workspace/tool-output/"] {
            assert!(
                clipped.contains(expected),
                "the note should mention {expected:?}: {}",
                &clipped[clipped.len().saturating_sub(400)..]
            );
        }
        // And it goes last, where a reader ends up.
        assert!(clipped.trim_end().ends_with(']'));
    }

    #[test]
    fn clip_leaves_short_text_untouched() {
        assert_eq!(clip("hello", 10), "hello");
    }

    #[test]
    fn clip_truncates_on_a_char_boundary_not_a_byte_boundary() {
        // Each of these is a multi-byte UTF-8 character; slicing by byte
        // count would panic or produce invalid UTF-8.
        let text = "café".repeat(20);
        let clipped = clip(&text, 5);
        assert!(clipped.ends_with("..."));
        assert_eq!(clipped.chars().count(), 8); // 5 kept + "..."
    }
}
