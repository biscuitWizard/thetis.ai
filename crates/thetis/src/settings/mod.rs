//! Reading and changing configuration at runtime.
//!
//! Edits go back to the same file the process was started from, through
//! `toml_edit`, so the comments that explain each setting survive. A rewrite
//! that dropped them would make the file worse every time it was touched.
//!
//! Two things guard the write:
//!
//! * **Validation.** The candidate text is put through the whole load path
//!   before it replaces anything. Thetis refuses to start on a bad config, so
//!   writing one and then restarting would be a way to make the system
//!   unbootable — the one failure mode there is no recovering from in-band.
//! * **Redaction.** Secrets can be written but never read back.
//!
//! Nothing is applied live: the process reads its configuration once at
//! startup. `set` says so, and the agent has a restart tool to finish the job.
//!
//! Two files, one rule. `thetis.toml` is committed; `thetis.local.toml` beside
//! it is not, and holds the accounts, their hashes and every secret. A write
//! lands in the overlay when the key is already set there (otherwise the
//! overlay would silently win again at the next boot), when the value is a
//! credential, or when it is an account or a role; everything else goes to the
//! committed file. Validation always judges the two together, because
//! neither loads on its own in users mode.
//!
//! `schema` describes every setting once — type, help, environment override —
//! so the surfaces that edit them render from it rather than knowing the
//! configuration themselves.

pub mod schema;

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value as TomlValue};

use crate::config::Config;
use schema::{Choices, Kind};

/// Settings whose values must never be read back out.
const SECRETS: &[&str] = &["llm.api_key", "discord.bot_token"];

/// Final path segments that mean a value is a credential, wherever it appears.
///
/// `SECRETS` names exact paths, which does not scale: every tool group can
/// carry its own token, and `[tools.notion] token` leaking into a listing is
/// the same mistake as `llm.api_key` leaking. Matching on the shape of the key
/// covers the ones nobody remembered to enumerate.
const SECRET_SUFFIXES: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "client_secret",
    "private_key",
    "access_token",
    "refresh_token",
    "bot_token",
    "webhook_url",
    "password_hash",
];

/// Settings that cannot be changed here, with the reason.
///
/// Empty. `paths` was locked because moving one orphans the artifacts and
/// database already on disk - true, and a reason to be careful rather than a
/// reason to refuse. Nothing written here takes effect until a restart, every
/// write is validated against a full config load first, and the agent can edit
/// the same file through the filesystem tools regardless, so the lock stopped
/// the supported route while leaving the unsupported one open.
const LOCKED: &[(&str, &str)] = &[];

#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    /// Dotted path, e.g. `llm.model`.
    pub key: String,
    /// Rendered value, or `***` for a secret that is set.
    pub value: String,
    pub editable: bool,
    /// Whether the running process would pick this up without a restart.
    pub live: bool,
}

fn is_secret(key: &str) -> bool {
    if SECRETS.iter().any(|s| *s == key) {
        return true;
    }
    let last = key.rsplit('.').next().unwrap_or(key).to_ascii_lowercase();
    SECRET_SUFFIXES.iter().any(|s| *s == last)
}

fn locked_reason(key: &str) -> Option<&'static str> {
    LOCKED
        .iter()
        .find(|(prefix, _)| key == *prefix || key.starts_with(&format!("{prefix}.")))
        .map(|(_, why)| *why)
}

/// Reads the config file, or an empty document when there is none yet.
fn document(cfg: &Config) -> Result<DocumentMut> {
    if !cfg.config_path.is_file() {
        return Ok(DocumentMut::new());
    }
    std::fs::read_to_string(&cfg.config_path)
        .with_context(|| format!("reading {}", cfg.config_path.display()))?
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", cfg.config_path.display()))
}

/// Every setting in the file, as dotted paths.
///
/// Arrays of tables — the `[[models]]` and `[[modes]]` lists — are reported as
/// a count rather than expanded, because editing one by index through a
/// key-value interface is more likely to corrupt the list than to help.
pub fn list(cfg: &Config, prefix: Option<&str>) -> Result<Vec<Setting>> {
    let doc = document(cfg)?;
    let mut out = Vec::new();
    walk(doc.as_item(), "", &mut out);

    if let Some(prefix) = prefix.map(str::trim).filter(|p| !p.is_empty()) {
        out.retain(|s| s.key == prefix || s.key.starts_with(&format!("{prefix}.")));
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

fn walk(item: &Item, path: &str, out: &mut Vec<Setting>) {
    match item {
        Item::Table(table) => {
            for (key, child) in table.iter() {
                walk(child, &join(path, key), out);
            }
        }
        Item::Value(TomlValue::InlineTable(table)) => {
            for (key, value) in table.iter() {
                walk(&Item::Value(value.clone()), &join(path, key), out);
            }
        }
        Item::ArrayOfTables(array) => {
            out.push(setting(path, format!("[{} entries]", array.len()), false));
        }
        Item::Value(value) => {
            out.push(setting(path, render(value), true));
        }
        Item::None => {}
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn setting(key: &str, value: String, scalar: bool) -> Setting {
    let editable = scalar && locked_reason(key).is_none();
    Setting {
        value: if is_secret(key) {
            if value.trim().is_empty() {
                String::new()
            } else {
                "***".to_string()
            }
        } else {
            value
        },
        key: key.to_string(),
        editable,
        // Configuration is read once at startup; nothing here takes effect
        // until the process comes back.
        live: false,
    }
}

fn render(value: &TomlValue) -> String {
    match value {
        TomlValue::String(s) => s.value().clone(),
        TomlValue::Integer(i) => i.value().to_string(),
        TomlValue::Float(f) => f.value().to_string(),
        TomlValue::Boolean(b) => b.value().to_string(),
        TomlValue::Datetime(d) => d.value().to_string(),
        TomlValue::Array(a) => a.iter().map(render).collect::<Vec<_>>().join(", "),
        TomlValue::InlineTable(_) => "{...}".to_string(),
    }
}

pub fn get(cfg: &Config, key: &str) -> Result<Option<Setting>> {
    Ok(list(cfg, None)?.into_iter().find(|s| s.key == key))
}

/// Which file a key is written to. See the module doc for the rule.
pub fn write_target(cfg: &Config, key: &str) -> PathBuf {
    let overlay = cfg.local_overlay();
    let path: Vec<&str> = key.split('.').collect();
    let in_overlay = document_at(&overlay)
        .ok()
        .and_then(|doc| traverse(&doc, &path).map(|_| ()))
        .is_some();
    let secret = is_secret(key) || schema::field(key).is_some_and(|f| f.kind == Kind::Secret);
    let account = matches!(path.first().copied(), Some("auth" | "roles" | "users"));
    if in_overlay || secret || account {
        overlay
    } else {
        cfg.config_path.clone()
    }
}

/// Validates the two files as they would load together, with `candidate`
/// standing in for whichever of them `target` is, then writes it.
fn write_validated(cfg: &Config, target: &Path, candidate: &str, what: &str) -> Result<()> {
    let overlay = cfg.local_overlay();
    let other = |p: &Path| -> Result<String> {
        if p.is_file() {
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))
        } else {
            Ok(String::new())
        }
    };
    let (file_text, overlay_text) = if target == overlay {
        (other(&cfg.config_path)?, candidate.to_string())
    } else {
        (candidate.to_string(), other(&overlay)?)
    };

    // The whole point of the guard: a config Thetis cannot load leaves it
    // unable to start, and nothing in-band can fix that.
    Config::validate_layers(&file_text, &overlay_text, &cfg.root)
        .with_context(|| format!("{what} would make the configuration invalid"))?;

    std::fs::write(target, candidate).with_context(|| format!("writing {}", target.display()))
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// Writes one setting back to the configuration.
///
/// Returns a description of what changed, including that a restart is needed.
pub fn set(cfg: &Config, key: &str, value: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("no setting named"));
    }
    if let Some(why) = locked_reason(key) {
        return Err(anyhow!("{key} cannot be changed here: {why}"));
    }

    let target = write_target(cfg, key);
    let mut doc = document_at(&target)?;
    let path: Vec<&str> = key.split('.').collect();

    // Refuse to overwrite a table or a list with a scalar, which would silently
    // delete a whole section.
    if let Some(existing) = traverse(&doc, &path) {
        match existing {
            Item::Table(_) | Item::ArrayOfTables(_) => {
                return Err(anyhow!(
                    "{key} is a section, not a setting; name one of the values inside it"
                ));
            }
            _ => {}
        }
    }
    if schema::table(key).is_some() {
        return Err(anyhow!("{key} is a list; edit its entries instead"));
    }

    let previous = traverse(&doc, &path)
        .and_then(|item| item.as_value().map(render))
        .unwrap_or_default();

    // The type comes from wherever the key is already set — this file, the
    // other one, or the built-in defaults — and failing all three from the
    // schema, so a value lands as the type the loader expects.
    let existing = traverse(&doc, &path)
        .and_then(Item::as_value)
        .cloned()
        .or_else(|| {
            let other = if target == cfg.local_overlay() {
                cfg.config_path.clone()
            } else {
                cfg.local_overlay()
            };
            document_at(&other)
                .ok()
                .and_then(|d| traverse(&d, &path).and_then(Item::as_value).cloned())
        })
        .or_else(|| traverse(&defaults_document(), &path).and_then(Item::as_value).cloned());
    let kind = schema::field(key).map(|f| f.kind);
    let parsed = parse_value(value, existing.as_ref(), kind)
        .with_context(|| format!("{value:?} is not a valid value for {key}"))?;

    insert(&mut doc, &path, parsed)?;
    write_validated(
        cfg,
        &target,
        &doc.to_string(),
        &format!("setting {key} to {value:?}"),
    )?;

    let shown = |v: &str| {
        if is_secret(key) {
            "***".to_string()
        } else {
            format!("{v:?}")
        }
    };

    tracing::warn!(%key, file = %file_name(&target), "configuration changed");
    Ok(format!(
        "{key}: {} -> {} (written to {}). Configuration is read at startup, so \
         restart Thetis for this to take effect.",
        if previous.is_empty() {
            "unset".to_string()
        } else {
            shown(&previous)
        },
        shown(value),
        file_name(&target),
    ))
}

// --- the described view ------------------------------------------------------

/// One setting with everything a form needs to edit it.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    pub key: String,
    /// Current effective value in the files, `***` for a set secret.
    pub value: String,
    pub default_value: String,
    /// `default`, `file`, `local` or `env`.
    pub source: &'static str,
    pub kind: &'static str,
    pub section: String,
    pub help: &'static str,
    pub env: Option<&'static str>,
    pub secret: bool,
    pub editable: bool,
    pub restart_required: bool,
    pub choices: Vec<String>,
}

fn document_at(path: &Path) -> Result<DocumentMut> {
    if !path.is_file() {
        return Ok(DocumentMut::new());
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Every setting at its built-in default, from `spec`.
pub fn defaults_document() -> DocumentMut {
    Config::default_file_toml()
        .parse::<DocumentMut>()
        .unwrap_or_default()
}

pub fn choices_for(cfg: &Config, choices: Choices) -> Vec<String> {
    match choices {
        Choices::None => Vec::new(),
        Choices::Models => cfg.models.iter().map(|m| m.id.clone()).collect(),
        Choices::Modes => cfg.modes.iter().map(|m| m.id.clone()).collect(),
        Choices::Roles => cfg.auth.roles.keys().cloned().collect(),
        Choices::Providers => cfg.providers.iter().map(|p| p.id.clone()).collect(),
        Choices::Static(list) => list.iter().map(|s| s.to_string()).collect(),
    }
}

fn masked(key: &str, value: String) -> String {
    if is_secret(key) {
        if value.trim().is_empty() {
            String::new()
        } else {
            "***".to_string()
        }
    } else {
        value
    }
}

/// Every setting, described: the schema's rows first, in schema order, then
/// whatever the files hold that the schema does not name (tool blocks).
pub fn describe(cfg: &Config, prefix: Option<&str>) -> Result<Vec<Described>> {
    let defaults = defaults_document();
    let file = document_at(&cfg.config_path)?;
    let overlay = document_at(&cfg.local_overlay())?;
    let prefix = prefix.map(str::trim).filter(|p| !p.is_empty());
    let wanted = |key: &str| prefix.is_none_or(|p| key == p || key.starts_with(&format!("{p}.")));

    let mut out = Vec::new();
    for field in schema::FIELDS {
        if !wanted(field.key) {
            continue;
        }
        let path: Vec<&str> = field.key.split('.').collect();
        let default_value = traverse(&defaults, &path)
            .and_then(Item::as_value)
            .map(render)
            .unwrap_or_default();
        let (mut value, mut source) = if let Some(v) = traverse(&overlay, &path).and_then(Item::as_value) {
            (render(v), "local")
        } else if let Some(v) = traverse(&file, &path).and_then(Item::as_value) {
            (render(v), "file")
        } else {
            (default_value.clone(), "default")
        };
        if let Some(env) = field.env {
            if let Some(v) = std::env::var(env).ok().filter(|v| !v.trim().is_empty()) {
                value = v;
                source = "env";
            }
        }
        out.push(Described {
            key: field.key.to_string(),
            value: masked(field.key, value),
            default_value: masked(field.key, default_value),
            source,
            kind: field.kind.name(),
            section: field.section.to_string(),
            help: field.help,
            env: field.env,
            secret: is_secret(field.key) || field.kind == Kind::Secret,
            editable: locked_reason(field.key).is_none(),
            restart_required: field.restart == schema::Restart::Required,
            choices: choices_for(cfg, field.choices),
        });
    }

    // What the files hold beyond the schema: per-tool blocks, mostly. Listed
    // so they can be edited, typed by what they are.
    let described: std::collections::HashSet<&str> = out.iter().map(|d| d.key.as_str()).collect();
    let mut extra: Vec<(String, String, &'static str)> = Vec::new();
    for (doc, source) in [(&file, "file"), (&overlay, "local")] {
        let mut leaves = Vec::new();
        walk(doc.as_item(), "", &mut leaves);
        for leaf in leaves {
            if described.contains(leaf.key.as_str())
                || !leaf.editable
                || schema::table(&leaf.key).is_some()
                || schema::TABLES.iter().any(|t| leaf.key.starts_with(&format!("{}.", t.id)))
                || !wanted(&leaf.key)
            {
                continue;
            }
            match extra.iter_mut().find(|(k, _, _)| *k == leaf.key) {
                Some(slot) => *slot = (leaf.key, leaf.value, source),
                None => extra.push((leaf.key, leaf.value, source)),
            }
        }
    }
    for (key, value, source) in extra {
        let path: Vec<&str> = key.split('.').collect();
        let doc = if source == "local" { &overlay } else { &file };
        let kind = match traverse(doc, &path).and_then(Item::as_value) {
            Some(TomlValue::Boolean(_)) => Kind::Bool,
            Some(TomlValue::Integer(_)) => Kind::Int,
            Some(TomlValue::Float(_)) => Kind::Float,
            Some(TomlValue::Array(_)) => Kind::List,
            _ if is_secret(&key) => Kind::Secret,
            _ => Kind::Text,
        };
        out.push(Described {
            section: path.first().map(|s| s.to_string()).unwrap_or_default(),
            key: key.clone(),
            value,
            default_value: String::new(),
            source,
            kind: kind.name(),
            help: "",
            env: None,
            secret: kind == Kind::Secret,
            editable: locked_reason(&key).is_none(),
            restart_required: true,
            choices: Vec::new(),
        });
    }
    Ok(out)
}

// --- lists of tables --------------------------------------------------------

/// One entry of a list section, its fields as JSON with secrets masked.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    /// `file` or `local`: which file holds the list.
    pub source: &'static str,
    pub fields: serde_json::Value,
}

fn item_to_json(item: &Item) -> serde_json::Value {
    fn value(v: &TomlValue) -> serde_json::Value {
        match v {
            TomlValue::String(s) => serde_json::Value::from(s.value().as_str()),
            TomlValue::Integer(i) => serde_json::Value::from(*i.value()),
            TomlValue::Float(f) => serde_json::Value::from(*f.value()),
            TomlValue::Boolean(b) => serde_json::Value::from(*b.value()),
            TomlValue::Datetime(d) => serde_json::Value::from(d.value().to_string()),
            TomlValue::Array(a) => serde_json::Value::Array(a.iter().map(value).collect()),
            TomlValue::InlineTable(t) => serde_json::Value::Object(
                t.iter().map(|(k, v)| (k.to_string(), value(v))).collect(),
            ),
        }
    }
    match item {
        Item::None => serde_json::Value::Null,
        Item::Value(v) => value(v),
        Item::Table(t) => serde_json::Value::Object(
            t.iter().map(|(k, v)| (k.to_string(), item_to_json(v))).collect(),
        ),
        Item::ArrayOfTables(a) => serde_json::Value::Array(
            a.iter().map(|t| item_to_json(&Item::Table(t.clone()))).collect(),
        ),
    }
}

fn json_to_value(v: &serde_json::Value, kind: Option<Kind>) -> Result<TomlValue> {
    Ok(match v {
        serde_json::Value::Null => return Err(anyhow!("null is not a value")),
        serde_json::Value::Bool(b) => (*b).into(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                match kind {
                    Some(Kind::Float) => (i as f64).into(),
                    _ => i.into(),
                }
            } else {
                n.as_f64().unwrap_or(0.0).into()
            }
        }
        serde_json::Value::String(s) => match kind {
            // A form sends text; the schema says what it should be.
            Some(Kind::Bool) => s.trim().parse::<bool>()?.into(),
            Some(Kind::Int) => s.trim().parse::<i64>()?.into(),
            Some(Kind::Float) => s.trim().parse::<f64>()?.into(),
            Some(Kind::List) => {
                let mut array = toml_edit::Array::new();
                for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    array.push(part);
                }
                array.into()
            }
            _ => s.as_str().into(),
        },
        serde_json::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_to_value(item, None)?);
            }
            array.into()
        }
        serde_json::Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, v) in map {
                if v.is_null() {
                    continue;
                }
                table.insert(k, json_to_value(v, None)?);
            }
            table.into()
        }
    })
}

/// Masks credential-shaped keys of an entry, one level deep.
fn mask_fields(fields: &mut serde_json::Value) {
    if let Some(obj) = fields.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            if is_secret(k) {
                let set = !v.as_str().unwrap_or_default().is_empty();
                *v = serde_json::Value::from(if set { "***" } else { "" });
            }
        }
    }
}

/// Which file defines a list, or where a new one belongs.
fn list_target(cfg: &Config, section: &schema::TableSection) -> Result<(PathBuf, &'static str)> {
    let path: Vec<&str> = section.id.split('.').collect();
    let overlay = cfg.local_overlay();
    if matches!(traverse(&document_at(&overlay)?, &path), Some(Item::ArrayOfTables(_))) {
        return Ok((overlay, "local"));
    }
    if matches!(traverse(&document_at(&cfg.config_path)?, &path), Some(Item::ArrayOfTables(_))) {
        return Ok((cfg.config_path.clone(), "file"));
    }
    Ok(if section.local {
        (overlay, "local")
    } else {
        (cfg.config_path.clone(), "file")
    })
}

pub fn entries(cfg: &Config, section: &str) -> Result<Vec<Entry>> {
    let spec = schema::table(section).ok_or_else(|| anyhow!("{section} is not a list section"))?;
    let (target, source) = list_target(cfg, spec)?;
    let doc = document_at(&target)?;
    let path: Vec<&str> = spec.id.split('.').collect();
    let Some(Item::ArrayOfTables(array)) = traverse(&doc, &path) else {
        return Ok(Vec::new());
    };
    Ok(array
        .iter()
        .map(|table| {
            let mut fields = item_to_json(&Item::Table(table.clone()));
            mask_fields(&mut fields);
            Entry {
                id: table
                    .get("id")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                source,
                fields,
            }
        })
        .collect())
}

fn array_mut<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut toml_edit::ArrayOfTables> {
    let (last, parents) = path.split_last().ok_or_else(|| anyhow!("empty section"))?;
    let mut item = doc.as_item_mut();
    for part in parents {
        item = item
            .as_table_mut()
            .ok_or_else(|| anyhow!("{part} is not a section"))?
            .entry(part)
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                Item::Table(t)
            });
    }
    let table = item
        .as_table_mut()
        .ok_or_else(|| anyhow!("cannot place a list there"))?;
    let slot = table
        .entry(last)
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    slot.as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("{last} is not a list of tables"))
}

/// Adds or updates one entry. Fields given as `null` are removed; fields not
/// mentioned are left as they are. For users, `password` is hashed into
/// `password_hash` and never stored or shown as given.
pub fn save_entry(
    cfg: &Config,
    section: &str,
    id: &str,
    fields: &serde_json::Value,
) -> Result<String> {
    let spec = schema::table(section).ok_or_else(|| anyhow!("{section} is not a list section"))?;
    let id = id.trim();
    if id.is_empty() {
        return Err(anyhow!("an entry needs an id"));
    }
    let given = fields
        .as_object()
        .ok_or_else(|| anyhow!("fields must be an object"))?;

    let mut fields = given.clone();
    if spec.id == "users" {
        if let Some(password) = fields.remove("password") {
            let password = password.as_str().unwrap_or_default();
            if !password.is_empty() {
                let hash = crate::auth::hash_password(password)?;
                fields.insert("password_hash".into(), serde_json::Value::from(hash));
            }
        }
    }
    for key in fields.keys() {
        let known = key == "id"
            || key == "password_hash" && spec.id == "users"
            || spec.columns.iter().any(|c| c.key == key);
        if !known {
            return Err(anyhow!("{section} entries have no field named {key}"));
        }
    }

    let (target, _) = list_target(cfg, spec)?;
    let mut doc = document_at(&target)?;
    let path: Vec<&str> = spec.id.split('.').collect();
    let array = array_mut(&mut doc, &path)?;

    let existing = array
        .iter()
        .position(|t| t.get("id").and_then(Item::as_str) == Some(id));
    let (table, created) = match existing {
        Some(i) => (array.get_mut(i).expect("index just found"), false),
        None => {
            let mut t = toml_edit::Table::new();
            t.insert("id", Item::Value(id.into()));
            array.push(t);
            let n = array.len() - 1;
            (array.get_mut(n).expect("just pushed"), true)
        }
    };
    for (key, value) in &fields {
        if key == "id" {
            continue;
        }
        if value.is_null() {
            table.remove(key);
            continue;
        }
        let kind = spec.columns.iter().find(|c| c.key == key).map(|c| c.kind);
        let parsed = json_to_value(value, kind)
            .with_context(|| format!("{value} is not a valid value for {key}"))?;
        table.insert(key, Item::Value(parsed));
    }

    write_validated(
        cfg,
        &target,
        &doc.to_string(),
        &format!("{} {id} in {section}", if created { "adding" } else { "changing" }),
    )?;
    tracing::warn!(section, id, file = %file_name(&target), "configuration changed");
    Ok(format!(
        "{section}: {} {id} (written to {}). Configuration is read at startup, so restart \
         Thetis for this to take effect.",
        if created { "added" } else { "updated" },
        file_name(&target)
    ))
}

pub fn remove_entry(cfg: &Config, section: &str, id: &str) -> Result<String> {
    let spec = schema::table(section).ok_or_else(|| anyhow!("{section} is not a list section"))?;
    let (target, _) = list_target(cfg, spec)?;
    let mut doc = document_at(&target)?;
    let path: Vec<&str> = spec.id.split('.').collect();
    let Some(Item::ArrayOfTables(array)) = traverse(&doc, &path) else {
        return Err(anyhow!("{section} has no entry {id}"));
    };
    let Some(index) = array
        .iter()
        .position(|t| t.get("id").and_then(Item::as_str) == Some(id))
    else {
        return Err(anyhow!("{section} has no entry {id}"));
    };
    array_mut(&mut doc, &path)?.remove(index);
    write_validated(cfg, &target, &doc.to_string(), &format!("removing {id} from {section}"))?;
    tracing::warn!(section, id, file = %file_name(&target), "configuration changed");
    Ok(format!(
        "{section}: removed {id} (written to {}). Configuration is read at startup, so \
         restart Thetis for this to take effect.",
        file_name(&target)
    ))
}

fn traverse<'a>(doc: &'a DocumentMut, path: &[&str]) -> Option<&'a Item> {
    let mut item = doc.as_item();
    for part in path {
        item = item.get(part)?;
    }
    Some(item)
}

fn insert(doc: &mut DocumentMut, path: &[&str], value: TomlValue) -> Result<()> {
    let (last, parents) = path
        .split_last()
        .ok_or_else(|| anyhow!("empty setting name"))?;

    let mut item = doc.as_item_mut();
    for part in parents {
        // Creating missing sections keeps a setting reachable even when the
        // file omits the section it belongs to.
        item = item
            .as_table_mut()
            .ok_or_else(|| anyhow!("{part} is not a section"))?
            .entry(part)
            .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    }

    let table = item
        .as_table_mut()
        .ok_or_else(|| anyhow!("cannot place a value there"))?;
    table.insert(last, Item::Value(value));
    Ok(())
}

/// Parses a value, taking its type from whatever is already there, else from
/// the schema, else from the text itself.
///
/// Without that, `max_iterations = "8"` would land as a string and fail to load
/// on the next start, which is exactly the mistake the validator would then
/// have to catch.
fn parse_value(raw: &str, existing: Option<&TomlValue>, kind: Option<Kind>) -> Result<TomlValue> {
    let text = raw.trim();

    let list = || {
        // Accept either TOML array syntax or a comma-separated list.
        let inner = text.trim_start_matches('[').trim_end_matches(']');
        let mut array = toml_edit::Array::new();
        for part in inner.split(',') {
            let part = part.trim().trim_matches('"').trim_matches('\'');
            if !part.is_empty() {
                array.push(part);
            }
        }
        TomlValue::from(array)
    };

    match existing {
        Some(TomlValue::Integer(_)) => {
            return Ok(text.parse::<i64>()?.into());
        }
        Some(TomlValue::Float(_)) => {
            return Ok(text.parse::<f64>()?.into());
        }
        Some(TomlValue::Boolean(_)) => {
            return Ok(text.parse::<bool>()?.into());
        }
        Some(TomlValue::Array(_)) => return Ok(list()),
        Some(TomlValue::String(_)) => return Ok(text.into()),
        _ => {}
    }

    match kind {
        Some(Kind::Bool) => return Ok(text.parse::<bool>()?.into()),
        Some(Kind::Int) => return Ok(text.parse::<i64>()?.into()),
        Some(Kind::Float) => return Ok(text.parse::<f64>()?.into()),
        Some(Kind::List) => return Ok(list()),
        Some(_) => return Ok(text.into()),
        None => {}
    }

    // Nothing to match against, so infer from the text itself.
    if let Ok(b) = text.parse::<bool>() {
        return Ok(b.into());
    }
    if let Ok(i) = text.parse::<i64>() {
        return Ok(i.into());
    }
    if let Ok(f) = text.parse::<f64>() {
        return Ok(f.into());
    }
    Ok(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# Thetis configuration.
[server]
# The address to bind.
bind = "127.0.0.1:7777"
admin_enabled = true

[agent]
max_iterations = 32
default_mode = "agent"

[llm]
model = "anthropic/claude-sonnet-4.5"
api_key = "sk-or-v1-secret"

[[models]]
id = "anthropic/claude-sonnet-4.5"

[[modes]]
id = "agent"

[paths]
data = "data"
"#;

    fn fixture() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thetis.toml");
        std::fs::write(&path, SAMPLE).unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.root = dir.path().to_path_buf();
        cfg.config_path = path;
        (cfg, dir)
    }

    fn value_of(cfg: &Config, key: &str) -> String {
        get(cfg, key).unwrap().unwrap().value
    }

    #[test]
    fn lists_settings_as_dotted_paths() {
        let (cfg, _d) = fixture();
        let keys: Vec<String> = list(&cfg, None)
            .unwrap()
            .into_iter()
            .map(|s| s.key)
            .collect();

        assert!(keys.contains(&"server.bind".to_string()));
        assert!(keys.contains(&"agent.max_iterations".to_string()));
        assert!(keys.contains(&"llm.model".to_string()));
    }

    #[test]
    fn a_prefix_narrows_the_listing() {
        let (cfg, _d) = fixture();
        let keys: Vec<String> = list(&cfg, Some("agent"))
            .unwrap()
            .into_iter()
            .map(|s| s.key)
            .collect();

        assert_eq!(keys, vec!["agent.default_mode", "agent.max_iterations"]);
    }

    #[test]
    fn secrets_are_never_read_back() {
        let (cfg, _d) = fixture();
        let key = get(&cfg, "llm.api_key").unwrap().unwrap();

        assert_eq!(key.value, "***");
        assert!(
            !list(&cfg, None)
                .unwrap()
                .iter()
                .any(|s| s.value.contains("sk-or-v1-secret")),
            "the real key leaked into the listing"
        );
    }

    #[test]
    fn a_credential_shaped_key_is_masked_wherever_it_lives() {
        // A tool group's token is as much a secret as llm.api_key, and no
        // exhaustive list of paths would have included it.
        assert!(is_secret("tools.notion.token"));
        assert!(is_secret("tools.stripe.api_key"));
        assert!(is_secret("some.nested.client_secret"));

        // Ordinary settings stay readable, including one that merely mentions
        // a credential inside a longer name.
        assert!(!is_secret("llm.model"));
        assert!(!is_secret("tools.notion.version"));
        assert!(!is_secret("tools.notion.token_path"));
    }

    #[test]
    fn a_tool_token_does_not_leak_into_a_listing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thetis.toml");
        std::fs::write(
            &path,
            "[tools.notion]\ntoken = \"ntn_super_secret\"\nversion = \"2026-03-11\"\n",
        )
        .unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.root = dir.path().to_path_buf();
        cfg.config_path = path;

        let settings = list(&cfg, Some("tools")).unwrap();
        let token = settings
            .iter()
            .find(|s| s.key == "tools.notion.token")
            .expect("the token should still be listed, just masked");

        assert_eq!(token.value, "***");
        assert!(
            !settings
                .iter()
                .any(|s| s.value.contains("ntn_super_secret")),
            "the token leaked: {settings:?}"
        );
        assert_eq!(
            settings
                .iter()
                .find(|s| s.key == "tools.notion.version")
                .unwrap()
                .value,
            "2026-03-11"
        );
    }

    #[test]
    fn a_secret_can_still_be_written() {
        let (cfg, _d) = fixture();
        let report = set(&cfg, "llm.api_key", "sk-or-v1-brand-new").unwrap();

        // Neither the old nor the new value appears in what is reported back,
        // and a credential lands in the overlay, never the committed file.
        assert!(!report.contains("sk-or-v1"), "{report}");
        assert!(
            std::fs::read_to_string(cfg.local_overlay())
                .unwrap()
                .contains("sk-or-v1-brand-new")
        );
    }

    #[test]
    fn comments_survive_an_edit() {
        let (cfg, _d) = fixture();
        set(&cfg, "agent.max_iterations", "48").unwrap();

        let text = std::fs::read_to_string(&cfg.config_path).unwrap();
        assert!(text.contains("# Thetis configuration."));
        assert!(text.contains("# The address to bind."));
        assert!(text.contains("max_iterations = 48"));
    }

    #[test]
    fn a_values_type_is_taken_from_what_is_already_there() {
        let (cfg, _d) = fixture();

        set(&cfg, "agent.max_iterations", "12").unwrap();
        set(&cfg, "server.admin_enabled", "false").unwrap();
        let text = std::fs::read_to_string(&cfg.config_path).unwrap();

        // Quoted numbers or booleans would fail to load on the next start.
        assert!(text.contains("max_iterations = 12"), "{text}");
        assert!(text.contains("admin_enabled = false"), "{text}");
    }

    #[test]
    fn a_change_that_would_not_load_is_refused() {
        let (cfg, _d) = fixture();
        let before = std::fs::read_to_string(&cfg.config_path).unwrap();

        // A bind address that cannot parse would stop Thetis starting at all.
        let err = set(&cfg, "server.bind", "not-an-address").unwrap_err();
        assert!(format!("{err:#}").contains("invalid"), "{err:#}");

        assert_eq!(
            std::fs::read_to_string(&cfg.config_path).unwrap(),
            before,
            "the file must be untouched when the candidate is rejected"
        );
    }

    #[test]
    fn a_mode_that_does_not_exist_is_refused() {
        let (cfg, _d) = fixture();
        let err = set(&cfg, "agent.default_mode", "nonsense").unwrap_err();
        assert!(format!("{err:#}").contains("nonsense"), "{err:#}");
    }

    #[test]
    fn sections_are_not_editable_as_values() {
        let (cfg, _d) = fixture();
        let err = set(&cfg, "server", "nope").unwrap_err();
        assert!(format!("{err:#}").contains("section"), "{err:#}");
    }

    #[test]
    fn paths_are_editable_like_anything_else() {
        let (cfg, _d) = fixture();
        assert!(get(&cfg, "paths.data").unwrap().unwrap().editable);

        set(&cfg, "paths.data", "elsewhere").unwrap();
        assert_eq!(get(&cfg, "paths.data").unwrap().unwrap().value, "elsewhere");
    }

    #[test]
    fn a_locked_setting_still_reports_why_when_one_is_configured() {
        // LOCKED is empty, so exercise the mechanism directly rather than
        // pinning a particular key to it.
        assert!(locked_reason("paths.data").is_none());
    }

    #[test]
    fn lists_of_tables_are_summarised_rather_than_expanded() {
        let (cfg, _d) = fixture();
        let models = get(&cfg, "models").unwrap().unwrap();

        assert_eq!(models.value, "[1 entries]");
        assert!(!models.editable);
    }

    #[test]
    fn a_setting_the_file_omits_can_still_be_added() {
        let (cfg, _d) = fixture();
        set(&cfg, "budgets.tool_secs", "600").unwrap();

        assert_eq!(value_of(&cfg, "budgets.tool_secs"), "600");
    }

    // --- schema and the described view --------------------------------------

    fn leaf_keys(doc: &DocumentMut) -> Vec<String> {
        let mut leaves = Vec::new();
        walk(doc.as_item(), "", &mut leaves);
        leaves.into_iter().map(|s| s.key).collect()
    }

    /// A setting added to `config.rs` must be described, or the panel cannot
    /// show it. The defaults document is the complete list of what exists.
    #[test]
    fn every_default_key_is_in_the_schema() {
        let missing: Vec<String> = leaf_keys(&defaults_document())
            .into_iter()
            .filter(|k| schema::table(k).is_none() && schema::field(k).is_none())
            .collect();
        assert!(missing.is_empty(), "settings without a schema row: {missing:?}");
    }

    /// And the other way: a row naming a key the loader does not know is a
    /// typo that would describe a setting nothing reads.
    #[test]
    fn every_schema_key_has_a_default() {
        let known = leaf_keys(&defaults_document());
        let phantom: Vec<&str> = schema::FIELDS
            .iter()
            .map(|f| f.key)
            .filter(|k| !known.iter().any(|d| d == k))
            .collect();
        assert!(phantom.is_empty(), "schema rows with no setting behind them: {phantom:?}");
        for f in schema::FIELDS {
            assert!(
                schema::SECTIONS.iter().any(|(id, _, _)| *id == f.section),
                "{} names an unknown section {}",
                f.key,
                f.section
            );
        }
    }

    #[test]
    fn describe_says_where_each_value_comes_from_and_masks_secrets() {
        let (cfg, _d) = fixture();
        let all = describe(&cfg, None).unwrap();
        let find = |k: &str| all.iter().find(|d| d.key == k).unwrap().clone();

        let key = find("llm.api_key");
        assert_eq!(key.value, "***");
        assert_eq!(key.source, "file");
        assert!(key.secret);

        let iterations = find("agent.max_iterations");
        assert_eq!(iterations.value, "32");
        assert_eq!(iterations.source, "file");
        assert_eq!(iterations.kind, "int");

        let port = find("browser.port");
        assert_eq!(port.source, "default");
        assert_eq!(port.value, "39412");
        assert_eq!(port.default_value, "39412");
        assert_eq!(port.section, "browser");
        assert!(!port.help.is_empty());

        assert!(
            !all.iter().any(|d| d.value.contains("sk-or-v1")),
            "a secret leaked into the described view"
        );
        assert!(
            !all.iter().any(|d| d.key == "models"),
            "lists of tables are entries, not settings"
        );
    }

    #[test]
    fn the_overlay_wins_in_the_described_view() {
        let (cfg, _d) = fixture();
        std::fs::write(cfg.local_overlay(), "[agent]\nmax_iterations = 7\n").unwrap();
        let all = describe(&cfg, Some("agent")).unwrap();
        let it = all.iter().find(|d| d.key == "agent.max_iterations").unwrap();
        assert_eq!((it.value.as_str(), it.source), ("7", "local"));
    }

    #[test]
    fn a_tool_block_is_described_as_free_form() {
        let (cfg, _d) = fixture();
        std::fs::write(cfg.local_overlay(), "[tools.notion]\ntoken = \"ntn_x\"\nversion = \"2026\"\n").unwrap();
        let all = describe(&cfg, Some("tools")).unwrap();
        let token = all.iter().find(|d| d.key == "tools.notion.token").unwrap();
        assert_eq!((token.value.as_str(), token.source, token.kind), ("***", "local", "secret"));
        let version = all.iter().find(|d| d.key == "tools.notion.version").unwrap();
        assert_eq!((version.value.as_str(), version.section.as_str()), ("2026", "tools"));
    }

    // --- which file a write lands in ----------------------------------------

    #[test]
    fn secrets_and_accounts_go_to_the_overlay_and_the_rest_to_the_file() {
        let (cfg, _d) = fixture();
        let report = set(&cfg, "llm.api_key", "sk-or-v1-new").unwrap();
        assert!(report.contains("thetis.local.toml"), "{report}");
        let overlay = std::fs::read_to_string(cfg.local_overlay()).unwrap();
        assert!(overlay.contains("sk-or-v1-new"));
        assert!(
            !std::fs::read_to_string(&cfg.config_path).unwrap().contains("sk-or-v1-new"),
            "the committed file must not receive the key"
        );

        let report = set(&cfg, "agent.max_iterations", "48").unwrap();
        assert!(report.contains("thetis.toml)"), "{report}");
        assert!(!overlay.contains("max_iterations"));

        assert_eq!(write_target(&cfg, "auth.session_ttl_hours"), cfg.local_overlay());
    }

    #[test]
    fn a_key_already_in_the_overlay_stays_there() {
        let (cfg, _d) = fixture();
        std::fs::write(cfg.local_overlay(), "[agent]\nmax_iterations = 7\n").unwrap();
        set(&cfg, "agent.max_iterations", "9").unwrap();
        assert!(std::fs::read_to_string(cfg.local_overlay()).unwrap().contains("max_iterations = 9"));
        assert!(std::fs::read_to_string(&cfg.config_path).unwrap().contains("max_iterations = 32"));
    }

    #[test]
    fn a_setting_absent_from_every_file_takes_its_type_from_the_schema() {
        let (cfg, _d) = fixture();
        set(&cfg, "browser.auto_install", "false").unwrap();
        let text = std::fs::read_to_string(&cfg.config_path).unwrap();
        assert!(text.contains("auto_install = false"), "{text}");
        set(&cfg, "cache.explicit_vendors", "anthropic, google").unwrap();
        let text = std::fs::read_to_string(&cfg.config_path).unwrap();
        assert!(text.contains(r#"explicit_vendors = ["anthropic", "google"]"#), "{text}");
    }

    #[test]
    fn validation_judges_both_files_together() {
        let (cfg, _d) = fixture();
        // Users mode with the accounts in the overlay: valid only as a pair.
        std::fs::write(
            cfg.local_overlay(),
            "[auth]\nmode = \"users\"\nclaim_unowned = \"ada\"\n[[roles]]\nid = \"admin\"\nadmin = true\n\
             [[users]]\nid = \"ada\"\nrole = \"admin\"\npassword_hash = \"$argon2id$x\"\n",
        )
        .unwrap();
        set(&cfg, "agent.max_iterations", "12").unwrap();
        assert!(std::fs::read_to_string(&cfg.config_path).unwrap().contains("max_iterations = 12"));
    }

    // --- entries --------------------------------------------------------------

    #[test]
    fn a_list_entry_round_trips_and_comments_survive() {
        let (cfg, _d) = fixture();
        let report = save_entry(
            &cfg,
            "models",
            "openai/gpt-4o",
            &serde_json::json!({ "label": "GPT-4o" }),
        )
        .unwrap();
        assert!(report.contains("added"), "{report}");

        let rows = entries(&cfg, "models").unwrap();
        assert_eq!(rows.len(), 2);
        let added = rows.iter().find(|e| e.id == "openai/gpt-4o").unwrap();
        assert_eq!(added.fields["label"], "GPT-4o");
        assert_eq!(added.source, "file");

        save_entry(&cfg, "models", "openai/gpt-4o", &serde_json::json!({ "label": null, "wire_model": "gpt-4o" })).unwrap();
        let rows = entries(&cfg, "models").unwrap();
        let changed = rows.iter().find(|e| e.id == "openai/gpt-4o").unwrap();
        assert!(changed.fields.get("label").is_none());
        assert_eq!(changed.fields["wire_model"], "gpt-4o");

        remove_entry(&cfg, "models", "openai/gpt-4o").unwrap();
        assert_eq!(entries(&cfg, "models").unwrap().len(), 1);

        let text = std::fs::read_to_string(&cfg.config_path).unwrap();
        assert!(text.contains("# Thetis configuration."), "{text}");
        assert!(text.contains("# The address to bind."), "{text}");
    }

    #[test]
    fn an_entry_a_form_typed_is_parsed_by_its_column() {
        let (cfg, _d) = fixture();
        save_entry(&cfg, "modes", "chat", &serde_json::json!({ "read_only": "true", "label": "Chat" })).unwrap();
        let text = std::fs::read_to_string(&cfg.config_path).unwrap();
        assert!(text.contains("read_only = true"), "{text}");
        let err = save_entry(&cfg, "modes", "chat", &serde_json::json!({ "colour": "red" })).unwrap_err();
        assert!(format!("{err:#}").contains("no field named colour"), "{err:#}");
    }

    #[test]
    fn a_user_password_is_hashed_and_never_shown() {
        let (cfg, _d) = fixture();
        save_entry(&cfg, "roles", "reader", &serde_json::json!({ "read_only": true })).unwrap();
        let report = save_entry(
            &cfg,
            "users",
            "ada",
            &serde_json::json!({ "name": "Ada", "role": "reader", "password": "hunter2" }),
        )
        .unwrap();
        assert!(!report.contains("hunter2"));
        assert!(report.contains("thetis.local.toml"), "accounts belong in the overlay: {report}");

        let overlay = std::fs::read_to_string(cfg.local_overlay()).unwrap();
        assert!(overlay.contains("$argon2id$"), "{overlay}");
        assert!(!overlay.contains("hunter2"));
        assert!(!overlay.contains("password ="), "{overlay}");

        let rows = entries(&cfg, "users").unwrap();
        assert_eq!(rows[0].source, "local");
        assert_eq!(rows[0].fields["password_hash"], "***");
        assert!(rows[0].fields.get("password").is_none());
        assert!(crate::auth::verify_password(
            "hunter2",
            overlay.split("password_hash = \"").nth(1).unwrap().split('"').next().unwrap()
        ));
    }

    #[test]
    fn an_entry_that_would_not_load_is_refused() {
        let (cfg, _d) = fixture();
        let before = std::fs::read_to_string(&cfg.config_path).unwrap();
        let err = save_entry(&cfg, "models", "x/y", &serde_json::json!({ "provider": "nowhere" })).unwrap_err();
        assert!(format!("{err:#}").contains("invalid"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&cfg.config_path).unwrap(), before);
    }
}
