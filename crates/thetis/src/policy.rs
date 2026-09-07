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
    /// The narrower of two policies, field by field.
    ///
    /// This is what makes a shared conversation safe. A turn's authority is
    /// `policy(speaker) ∩ ceiling(session)`, so neither speaking in someone
    /// else's conversation nor owning a conversation someone else speaks in can
    /// raise anybody above their own account. Every field composes towards
    /// *less*: grants are ANDed, denials unioned, limits minimised.
    ///
    /// Deliberately total rather than clever. Every field is named explicitly
    /// and `intersect_is_exhaustive_over_every_field` destructures the struct so
    /// that a field added later fails to compile here instead of silently
    /// passing through at whichever side happened to be wider.
    pub fn intersect(&self, other: &Self) -> Self {
        // Capability denial goes through `denies`, not the raw set, so that a
        // `read_only` side contributes its implied write denials to the result
        // as real entries rather than relying on the flag surviving.
        let denied: BTreeSet<Cap> = Cap::all()
            .iter()
            .copied()
            .filter(|cap| self.denies(*cap) || other.denies(*cap))
            .collect();

        // A model has to be allowed by both. Asking `allows_model` rather than
        // intersecting the lists is what keeps an unrestricted side from
        // wrongly narrowing a restricted one to the catalogue it happens to
        // list: unrestricted means "any id the provider will take".
        let mut models: Vec<String> = Vec::new();
        for id in self.models.iter().chain(other.models.iter()) {
            if !models.contains(id) && self.allows_model(id) && other.allows_model(id) {
                models.push(id.clone());
            }
        }
        let models_restricted = self.models_restricted || other.models_restricted;

        // Modes are always a closed list, so this is a plain intersection.
        let modes: Vec<String> = self
            .modes
            .iter()
            .filter(|m| other.modes.contains(m))
            .cloned()
            .collect();

        let default_model = first_allowed(
            &[&self.default_model, &other.default_model],
            |id| self.allows_model(id) && other.allows_model(id),
        )
        .or_else(|| models.first().cloned())
        .unwrap_or_default();
        let default_mode = first_allowed(&[&self.default_mode, &other.default_mode], |id| {
            modes.iter().any(|m| m == id)
        })
        .or_else(|| modes.first().cloned())
        .unwrap_or_default();

        Self {
            admin: self.admin && other.admin,
            read_only: self.read_only || other.read_only,
            denied,
            models,
            default_model,
            modes,
            default_mode,
            deny_tools: union_patterns(&self.deny_tools, &other.deny_tools),
            deny_groups: union_patterns(&self.deny_groups, &other.deny_groups),
            spend_limit_usd: tighter_limit(self.spend_limit_usd, other.spend_limit_usd),
            max_children: self.max_children.min(other.max_children),
            see_all_sessions: self.see_all_sessions && other.see_all_sessions,
            models_restricted,
        }
    }

    /// Whether this policy grants nothing `other` does not.
    ///
    /// The guard against a grant that widens. Nothing in `resolve` prevents a
    /// layer writing `admin = true`, so anywhere a policy is chosen rather than
    /// inherited — a fork's ceiling, a delegated child — the choice is checked
    /// against the authority making it.
    pub fn is_subset_of(&self, other: &Self) -> bool {
        if self.admin && !other.admin {
            return false;
        }
        if self.see_all_sessions && !other.see_all_sessions {
            return false;
        }
        // A read-only ceiling cannot be escaped by a policy that is not.
        if other.read_only && !self.read_only {
            return false;
        }
        // Everything the wider side denies, the narrower side must deny too.
        if Cap::all()
            .iter()
            .any(|cap| other.denies(*cap) && !self.denies(*cap))
        {
            return false;
        }
        if self.max_children > other.max_children {
            return false;
        }
        // Zero means no ceiling, so it is the widest value rather than the
        // narrowest. Comparing the numbers directly would read an unlimited
        // budget as the tightest one there is.
        match (self.spend_limit_usd, other.spend_limit_usd) {
            (_, o) if o <= 0.0 => {}
            (s, _) if s <= 0.0 => return false,
            (s, o) if s > o => return false,
            _ => {}
        }
        // Every model this policy would allow, the other must allow.
        if other.models_restricted {
            if !self.models_restricted {
                return false;
            }
            if self.models.iter().any(|m| !other.allows_model(m)) {
                return false;
            }
        }
        if self.modes.iter().any(|m| !other.allows_mode(m)) {
            return false;
        }
        // Pattern equality, not pattern subsumption: a conservative check that
        // can refuse a grant which is in fact narrow, but never admits one that
        // is wide.
        if other
            .deny_tools
            .iter()
            .any(|p| !self.deny_tools.contains(p))
        {
            return false;
        }
        if other
            .deny_groups
            .iter()
            .any(|p| !self.deny_groups.contains(p))
        {
            return false;
        }
        true
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

/// The first candidate a predicate accepts. Used to keep a default that both
/// sides of an intersection still permit.
fn first_allowed(candidates: &[&String], ok: impl Fn(&str) -> bool) -> Option<String> {
    candidates
        .iter()
        .find(|c| !c.is_empty() && ok(c))
        .map(|c| (*c).clone())
}

/// Every pattern either side denies, without duplicates. Denials union because
/// each one only ever removes something.
fn union_patterns(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for p in b {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// The tighter of two spend ceilings, where zero means "no ceiling".
fn tighter_limit(a: f64, b: f64) -> f64 {
    match (a, b) {
        (a, b) if a <= 0.0 && b <= 0.0 => 0.0,
        (a, b) if a <= 0.0 => b,
        (a, b) if b <= 0.0 => a,
        (a, b) => a.min(b),
    }
}

fn pattern_denies(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| {
        p.strip_suffix('*')
            .map_or(p == name, |prefix| name.starts_with(prefix))
    })
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    // --- intersection ------------------------------------------------------
    //
    // The rule the whole multi-user model rests on:
    //     effective(turn) = policy(speaker) ∩ ceiling(session)

    /// Destructures `EffectivePolicy` so that adding a field without deciding
    /// how it intersects is a compile error here rather than a silent hole. If
    /// this stops compiling, add the field to `intersect` and to this list —
    /// do not add `..` to the pattern.
    #[test]
    fn intersect_is_exhaustive_over_every_field() {
        let EffectivePolicy {
            admin: _,
            read_only: _,
            denied: _,
            models: _,
            default_model: _,
            modes: _,
            default_mode: _,
            deny_tools: _,
            deny_groups: _,
            spend_limit_usd: _,
            max_children: _,
            see_all_sessions: _,
            models_restricted: _,
        } = base();
    }

    #[test]
    fn intersect_takes_the_narrower_of_each_grant() {
        let mut wide = base();
        wide.admin = true;
        wide.see_all_sessions = true;
        wide.max_children = 8;
        wide.spend_limit_usd = 0.0; // unlimited

        let mut narrow = base();
        narrow.admin = false;
        narrow.see_all_sessions = false;
        narrow.max_children = 2;
        narrow.spend_limit_usd = 5.0;

        let got = wide.intersect(&narrow);
        assert!(!got.admin, "admin has to be granted by both sides");
        assert!(!got.see_all_sessions);
        assert_eq!(got.max_children, 2);
        assert_eq!(
            got.spend_limit_usd, 5.0,
            "an unlimited side must not erase a real ceiling"
        );
        // Order cannot matter, or the answer would depend on which side is
        // called the speaker.
        let flipped = narrow.intersect(&wide);
        assert_eq!(flipped.admin, got.admin);
        assert_eq!(flipped.max_children, got.max_children);
        assert_eq!(flipped.spend_limit_usd, got.spend_limit_usd);
    }

    /// Your question, as an assertion: a read-only speaker in a write-enabled
    /// conversation gets read-only.
    #[test]
    fn a_read_only_speaker_cannot_write_in_a_permissive_conversation() {
        let ceiling = base(); // write-enabled conversation
        let mut speaker = base();
        speaker.read_only = true;

        let got = speaker.intersect(&ceiling);
        assert!(got.read_only);
        assert!(got.denies(Cap::FilesystemWrite));
        assert!(got.denies(Cap::Terminal));
        assert!(got.denies(Cap::Devkit));
        assert!(!got.denies(Cap::FilesystemRead), "reading is still fine");
    }

    /// The same rule from the other direction: an admin speaking in a
    /// conversation whose ceiling is read-only — a Discord channel — gets
    /// read-only. This is what makes the Discord guarantee hard.
    #[test]
    fn an_admin_speaking_under_a_read_only_ceiling_gets_read_only() {
        // A Discord ceiling as `config.rs` actually builds it: read-only, not
        // admin, delegation denied.
        let mut ceiling = base();
        ceiling.read_only = true;
        ceiling.admin = false;
        ceiling.denied.insert(Cap::Delegation);

        let mut admin = base();
        admin.admin = true;

        let got = admin.intersect(&ceiling);
        assert!(got.read_only);
        assert!(!got.admin);
        assert!(got.denies(Cap::Devkit));
        assert!(got.denies(Cap::Delegation));
    }

    #[test]
    fn intersect_unions_denials_and_keeps_implied_ones() {
        let mut a = base();
        a.denied.insert(Cap::Transcripts);
        let mut b = base();
        b.read_only = true; // implies the write denials

        let got = a.intersect(&b);
        assert!(got.denied.contains(&Cap::Transcripts), "explicit denial");
        assert!(
            got.denied.contains(&Cap::FilesystemWrite),
            "a denial implied by read_only must be materialised in the set, \
             not left to depend on the flag"
        );
    }

    #[test]
    fn intersect_narrows_models_and_modes() {
        let mut a = base();
        a.models = vec!["a".into(), "b".into()];
        a.models_restricted = true;
        a.modes = vec!["agent".into(), "chat".into()];
        a.default_mode = "agent".into();

        let mut b = base();
        b.models = vec!["b".into(), "c".into()];
        b.models_restricted = true;
        b.modes = vec!["chat".into()];
        b.default_mode = "chat".into();

        let got = a.intersect(&b);
        assert_eq!(got.models, ["b"], "only what both allow");
        assert_eq!(got.default_model, "b", "a default both sides still permit");
        assert_eq!(got.modes, ["chat"]);
        assert_eq!(got.default_mode, "chat");
    }

    /// An unrestricted side means "any id the provider takes", so it must not
    /// silently narrow a restricted side down to its own catalogue.
    #[test]
    fn an_unrestricted_side_does_not_narrow_a_restricted_one() {
        let unrestricted = base(); // models_restricted = false
        let mut restricted = base();
        restricted.models = vec!["only-this".into()];
        restricted.models_restricted = true;

        let got = unrestricted.intersect(&restricted);
        assert!(got.models_restricted);
        assert!(got.allows_model("only-this"));
        assert!(!got.allows_model("something-else"));
    }

    // --- the subset invariant (H4) -----------------------------------------

    #[test]
    fn a_policy_is_a_subset_of_itself() {
        let p = base();
        assert!(p.is_subset_of(&p));
    }

    #[test]
    fn is_subset_of_refuses_every_way_of_widening() {
        let narrow = {
            let mut p = base();
            p.admin = false;
            p.see_all_sessions = false;
            p.read_only = true;
            p.max_children = 1;
            p.spend_limit_usd = 1.0;
            // Not a write capability: `read_only` already implies those, so
            // dropping one of them would not actually widen anything and the
            // assertion below would pass for the wrong reason.
            p.denied.insert(Cap::Transcripts);
            p.deny_tools = vec!["moo-*".into()];
            p.deny_groups = vec!["web".into()];
            p
        };

        // Each of these is the same policy with exactly one grant widened.
        let mut admin = narrow.clone();
        admin.admin = true;
        assert!(!admin.is_subset_of(&narrow), "admin");

        let mut see_all = narrow.clone();
        see_all.see_all_sessions = true;
        assert!(!see_all.is_subset_of(&narrow), "see_all_sessions");

        let mut writable = narrow.clone();
        writable.read_only = false;
        assert!(!writable.is_subset_of(&narrow), "read_only");

        let mut children = narrow.clone();
        children.max_children = 9;
        assert!(!children.is_subset_of(&narrow), "max_children");

        let mut unlimited = narrow.clone();
        unlimited.spend_limit_usd = 0.0;
        assert!(
            !unlimited.is_subset_of(&narrow),
            "zero means unlimited, which is wider than any real ceiling"
        );

        let mut undenied = narrow.clone();
        undenied.denied.remove(&Cap::Transcripts);
        assert!(!undenied.is_subset_of(&narrow), "a dropped capability denial");

        let mut tools = narrow.clone();
        tools.deny_tools.clear();
        assert!(!tools.is_subset_of(&narrow), "a dropped tool denial");

        let mut groups = narrow.clone();
        groups.deny_groups.clear();
        assert!(!groups.is_subset_of(&narrow), "a dropped group denial");
    }

    #[test]
    fn a_genuinely_narrower_policy_is_accepted() {
        let mut wide = base();
        wide.admin = true;
        wide.max_children = 8;

        let mut narrow = wide.clone();
        narrow.admin = false;
        narrow.read_only = true;
        narrow.max_children = 2;
        narrow.spend_limit_usd = 3.0;
        narrow.denied.insert(Cap::Transcripts);

        assert!(narrow.is_subset_of(&wide));
        assert!(!wide.is_subset_of(&narrow));
    }

    /// The two functions have to agree, or a ceiling could be applied and then
    /// fail its own check.
    #[test]
    fn an_intersection_is_always_a_subset_of_both_sides() {
        let mut a = base();
        a.admin = true;
        a.spend_limit_usd = 10.0;
        a.denied.insert(Cap::Transcripts);

        let mut b = base();
        b.read_only = true;
        b.max_children = 2;
        b.deny_groups = vec!["web".into()];

        let got = a.intersect(&b);
        assert!(got.is_subset_of(&a), "not a subset of the first side");
        assert!(got.is_subset_of(&b), "not a subset of the second side");
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
