//! Per-user authorization policy.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    FilesystemRead,
    FilesystemWrite,
    FilesystemDelete,
    Terminal,
    Ssh,
    Devkit,
    Control,
    ConfigWrite,
    BranchWrite,
    Delegation,
    SkillsWrite,
    Transcripts,
    ComponentTools,
    Sandbox,
    Workspace,
    WorkspaceWrite,
}

impl Cap {
    pub const fn all() -> &'static [Cap] {
        &[
            Cap::FilesystemRead,
            Cap::FilesystemWrite,
            Cap::FilesystemDelete,
            Cap::Terminal,
            Cap::Ssh,
            Cap::Devkit,
            Cap::Control,
            Cap::ConfigWrite,
            Cap::BranchWrite,
            Cap::Delegation,
            Cap::SkillsWrite,
            Cap::Transcripts,
            Cap::ComponentTools,
            Cap::Sandbox,
            Cap::Workspace,
            Cap::WorkspaceWrite,
        ]
    }
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.trim().to_owned())).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectivePolicy {
    pub admin: bool,
    pub read_only: bool,
    pub denied: BTreeSet<Cap>,
    pub models: Vec<String>,
    pub default_model: String,
    pub modes: Vec<String>,
    pub default_mode: String,
    pub deny_tools: Vec<String>,
    pub deny_groups: Vec<String>,
    pub spend_limit_usd: f64,
    pub max_children: usize,
    pub see_all_sessions: bool,
    pub models_restricted: bool,
}

impl EffectivePolicy {
    pub fn denies(&self, cap: Cap) -> bool {
        self.denied.contains(&cap)
            || self.read_only
                && matches!(
                    cap,
                    Cap::FilesystemWrite
                        | Cap::FilesystemDelete
                        | Cap::Terminal
                        | Cap::Ssh
                        | Cap::Devkit
                        | Cap::Control
                        | Cap::ConfigWrite
                        | Cap::BranchWrite
                        | Cap::SkillsWrite
                        | Cap::WorkspaceWrite
                )
    }
    /// Whether this user may run `id`.
    ///
    /// Unrestricted means unrestricted: any id the provider will take, not
    /// only the ones `[[models]]` happens to name. The configured catalogue is
    /// what the picker *offers*, never the set of models that exist — one
    /// added through the chat lives in the gateway's own overlay and is not in
    /// `[[models]]` at all. Holding an unrestricted user to that list meant
    /// editing configuration and restarting the process before a model could
    /// be tried once, and it broke conversations already running on a model
    /// added that way, which is how this was found: `openai/gpt-5.6-sol`, in
    /// use for a day, refused the moment accounts were turned on.
    ///
    /// A wrong id comes back from the provider as a clear error on the next
    /// turn, which is a better place to find out than a rejected click.
    ///
    /// A role or user that names `models` means it, and then the list is a
    /// closed set — that is the whole point of setting it.
    pub fn allows_model(&self, id: &str) -> bool {
        !self.models_restricted || self.models.iter().any(|v| v == id)
    }
    pub fn allows_mode(&self, id: &str) -> bool {
        self.modes.iter().any(|v| v == id)
    }
    pub fn denies_tool(&self, name: &str) -> bool {
        pattern_denies(&self.deny_tools, name)
    }
    pub fn denies_group(&self, id: &str) -> bool {
        self.deny_groups.iter().any(|v| v == id)
    }
    pub fn unrestricted(
        models: &[crate::config::ModelSpec],
        default_model: &str,
        modes: &[crate::config::ModeSpec],
        default_mode: &str,
        max_children: usize,
    ) -> Self {
        Self {
            admin: true,
            read_only: false,
            denied: BTreeSet::new(),
            models: models.iter().map(|v| v.id.clone()).collect(),
            default_model: default_model.into(),
            modes: modes.iter().map(|v| v.id.clone()).collect(),
            default_mode: default_mode.into(),
            deny_tools: vec![],
            deny_groups: vec![],
            spend_limit_usd: 0.0,
            max_children,
            see_all_sessions: false,
            models_restricted: false,
        }
    }
}

fn pattern_denies(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| {
        p.strip_suffix('*')
            .map_or(p == name, |prefix| name.starts_with(prefix))
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyLayer {
    pub admin: Option<bool>,
    pub read_only: Option<bool>,
    pub deny_capabilities: Option<Vec<String>>,
    pub models: Option<Vec<String>>,
    pub default_model: Option<String>,
    pub modes: Option<Vec<String>>,
    pub default_mode: Option<String>,
    pub deny_tools: Option<Vec<String>>,
    pub deny_groups: Option<Vec<String>>,
    pub spend_limit_usd: Option<f64>,
    pub max_children: Option<usize>,
    pub see_all_sessions: Option<bool>,
}

pub fn resolve(
    base: &EffectivePolicy,
    layers: &[&PolicyLayer],
    who: &str,
    all_models: &[String],
    all_modes: &[String],
) -> Result<EffectivePolicy> {
    let mut p = base.clone();
    for l in layers {
        if let Some(v) = l.admin {
            p.admin = v
        }
        if let Some(v) = l.read_only {
            p.read_only = v
        }
        if let Some(v) = &l.deny_capabilities {
            p.denied = v
                .iter()
                .map(|s| {
                    Cap::parse(s).ok_or_else(|| anyhow::anyhow!("{who}: unknown capability `{s}`"))
                })
                .collect::<Result<_>>()?
        }
        if let Some(v) = &l.models {
            p.models = v.clone();
            p.models_restricted = true
        }
        if let Some(v) = &l.default_model {
            p.default_model = v.clone()
        }
        if let Some(v) = &l.modes {
            p.modes = v.clone()
        }
        if let Some(v) = &l.default_mode {
            p.default_mode = v.clone()
        }
        if let Some(v) = &l.deny_tools {
            p.deny_tools = v.clone()
        }
        if let Some(v) = &l.deny_groups {
            p.deny_groups = v.clone()
        }
        if let Some(v) = l.spend_limit_usd {
            p.spend_limit_usd = v
        }
        if let Some(v) = l.max_children {
            p.max_children = v
        }
        if let Some(v) = l.see_all_sessions {
            p.see_all_sessions = v
        }
    }
    for v in &p.models {
        ensure!(
            all_models.contains(v),
            "{who}: model `{v}` is not in [[models]]"
        )
    }
    ensure!(!p.models.is_empty(), "{who}: no models would be offered");
    if !p.models.contains(&p.default_model) {
        tracing::warn!(who, "default model unavailable; using first");
        p.default_model = p.models[0].clone();
    }
    for v in &p.modes {
        ensure!(
            all_modes.contains(v),
            "{who}: mode `{v}` is not in [[modes]]"
        )
    }
    ensure!(!p.modes.is_empty(), "{who}: no modes would be offered");
    if !p.modes.contains(&p.default_mode) {
        p.default_mode = p.modes[0].clone();
    }
    ensure!(
        !p.deny_groups.iter().any(|v| v == "core"),
        "{who}: the `core` tool group cannot be denied"
    );
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> EffectivePolicy {
        EffectivePolicy {
            admin: true,
            read_only: false,
            denied: BTreeSet::new(),
            models: vec!["a".into(), "b".into()],
            default_model: "a".into(),
            modes: vec!["agent".into()],
            default_mode: "agent".into(),
            deny_tools: vec![],
            deny_groups: vec![],
            spend_limit_usd: 0.0,
            max_children: 4,
            see_all_sessions: false,
            models_restricted: false,
        }
    }
    // The configured catalogue is what the picker offers, not the set of
    // models that exist. A model added through the chat lives in the gateway's
    // overlay and is in no `[[models]]` block, so holding an unrestricted user
    // to that list broke a conversation that had been running on one for a day
    // the moment accounts were switched on.
    #[test]
    fn an_unrestricted_user_may_run_a_model_the_config_never_named() {
        let p = base();
        assert!(!p.models_restricted);
        assert!(p.allows_model("a"), "configured ones obviously still pass");
        assert!(
            p.allows_model("openai/gpt-5.6-sol"),
            "an id the catalogue does not name is for the provider to judge"
        );
    }

    // But a list that was set is a list that was meant.
    #[test]
    fn a_narrowed_list_is_a_closed_set() {
        let l = PolicyLayer {
            models: Some(vec!["a".into()]),
            ..Default::default()
        };
        let p = resolve(
            &base(),
            &[&l],
            "x",
            &["a".into(), "b".into()],
            &["agent".into()],
        )
        .unwrap();
        assert!(p.models_restricted);
        assert!(p.allows_model("a"));
        assert!(!p.allows_model("b"), "narrowing has to actually narrow");
        assert!(!p.allows_model("openai/gpt-5.6-sol"));
    }

    #[test]
    fn prefixes_and_read_only() {
        let mut p = base();
        p.deny_tools = vec!["moo-*".into()];
        p.read_only = true;
        assert!(p.denies_tool("moo-eval"));
        assert!(p.denies(Cap::Terminal));
        assert!(!p.denies(Cap::FilesystemRead));
    }
    #[test]
    fn layers_replace_lists() {
        let l = PolicyLayer {
            models: Some(vec!["b".into()]),
            ..Default::default()
        };
        let p = resolve(
            &base(),
            &[&l],
            "x",
            &["a".into(), "b".into()],
            &["agent".into()],
        )
        .unwrap();
        assert_eq!(p.models, ["b"]);
        assert_eq!(p.default_model, "b");
    }
    #[test]
    fn core_is_undeniable() {
        let l = PolicyLayer {
            deny_groups: Some(vec!["core".into()]),
            ..Default::default()
        };
        assert!(
            resolve(
                &base(),
                &[&l],
                "x",
                &["a".into(), "b".into()],
                &["agent".into()]
            )
            .is_err()
        );
    }
}
