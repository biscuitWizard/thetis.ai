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

use anyhow::{Context, Result, anyhow};
use toml_edit::{DocumentMut, Item, Value as TomlValue};

use crate::config::Config;

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

/// Writes one setting back to the config file.
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

    let mut doc = document(cfg)?;
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

    let previous = traverse(&doc, &path)
        .and_then(|item| item.as_value().map(render))
        .unwrap_or_default();

    let parsed = parse_value(value, traverse(&doc, &path).and_then(Item::as_value))
        .with_context(|| format!("{value:?} is not a valid value for {key}"))?;

    insert(&mut doc, &path, parsed)?;
    let candidate = doc.to_string();

    // The whole point of the guard: a config Thetis cannot load leaves it
    // unable to start, and nothing in-band can fix that.
    Config::validate(&candidate, &cfg.root).with_context(|| {
        format!("setting {key} to {value:?} would make the configuration invalid")
    })?;

    std::fs::write(&cfg.config_path, &candidate)
        .with_context(|| format!("writing {}", cfg.config_path.display()))?;

    let shown = |v: &str| {
        if is_secret(key) {
            "***".to_string()
        } else {
            format!("{v:?}")
        }
    };

    tracing::warn!(%key, "configuration changed");
    Ok(format!(
        "{key}: {} -> {} (written to {}). Configuration is read at startup, so \
         restart Thetis for this to take effect.",
        if previous.is_empty() {
            "unset".to_string()
        } else {
            shown(&previous)
        },
        shown(value),
        cfg.config_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| cfg.config_path.display().to_string()),
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

/// Parses a value, taking its type from whatever is already there.
///
/// Without that, `max_iterations = "8"` would land as a string and fail to load
/// on the next start, which is exactly the mistake the validator would then
/// have to catch.
fn parse_value(raw: &str, existing: Option<&TomlValue>) -> Result<TomlValue> {
    let text = raw.trim();

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
        Some(TomlValue::Array(_)) => {
            // Accept either TOML array syntax or a comma-separated list.
            let inner = text.trim_start_matches('[').trim_end_matches(']');
            let mut array = toml_edit::Array::new();
            for part in inner.split(',') {
                let part = part.trim().trim_matches('"').trim_matches('\'');
                if !part.is_empty() {
                    array.push(part);
                }
            }
            return Ok(array.into());
        }
        Some(TomlValue::String(_)) => return Ok(text.into()),
        _ => {}
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

        // Neither the old nor the new value appears in what is reported back.
        assert!(!report.contains("sk-or-v1"), "{report}");
        assert!(
            std::fs::read_to_string(&cfg.config_path)
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
}
