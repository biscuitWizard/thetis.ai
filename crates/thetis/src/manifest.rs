//! Dependency management for guest crates.
//!
//! A tool is only as capable as the crates it can reach, so the agent needs to
//! add dependencies to its own components. Letting it write `Cargo.toml`
//! wholesale would be the obvious way to allow that, and the wrong one: the
//! manifest also carries `[lib] crate-type = ["cdylib"]`, an empty `[workspace]`
//! stanza, and the `wit-bindgen` dependency, all of which have to survive for
//! the crate to build into a loadable component. One malformed rewrite and the
//! aspect stops being a component rather than merely stops compiling.
//!
//! So mutation is structured instead of textual. Everything here edits the
//! `[dependencies]` table and nothing else, leaving comments and the rest of
//! the file byte-identical.
//!
//! Registry dependencies only. `git` and `path` sources would let a dependency
//! be fetched from anywhere, or point back into the host filesystem, which is a
//! different question from "may this tool use a crate".

use anyhow::{bail, Context, Result};
use std::path::Path;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Value};

const MAX_NAME: usize = 64;
const MAX_VERSION: usize = 32;

/// One entry in a guest crate's `[dependencies]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    pub features: Vec<String>,
    pub default_features: bool,
}

/// Reads the `[dependencies]` table.
pub fn list(dir: &Path) -> Result<Vec<Dependency>> {
    let doc = read(dir)?;
    let Some(table) = doc.get("dependencies").and_then(Item::as_table_like) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for (name, item) in table.iter() {
        out.push(match item {
            // `foo = "1"`
            Item::Value(Value::String(v)) => Dependency {
                name: name.to_string(),
                version: v.value().to_string(),
                features: Vec::new(),
                default_features: true,
            },
            // `foo = { version = "1", features = [...] }`
            _ => {
                let detail = item.as_table_like();
                let get = |key: &str| detail.and_then(|t| t.get(key));
                Dependency {
                    name: name.to_string(),
                    version: get("version")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    features: get("features")
                        .and_then(|i| i.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                    default_features: get("default-features")
                        .and_then(|i| i.as_bool())
                        .unwrap_or(true),
                }
            }
        });
    }
    Ok(out)
}

/// Adds a dependency, replacing it if it is already present.
///
/// `allowed` restricts which crates may be named; empty means no restriction.
pub fn add(dir: &Path, dep: &Dependency, allowed: &[String]) -> Result<()> {
    validate_name(&dep.name)?;
    validate_version(&dep.version)?;
    for feature in &dep.features {
        validate_feature(feature)?;
    }
    if !allowed.is_empty() && !allowed.iter().any(|a| a == &dep.name) {
        bail!(
            "{} is not in the allowed crate list. Ask a human to add it to \
             build.allowed_crates in the config.",
            dep.name
        );
    }

    let mut doc = read(dir)?;
    let table = doc
        .entry("dependencies")
        .or_insert(Item::Table(Default::default()))
        .as_table_like_mut()
        .context("the manifest's [dependencies] is not a table")?;

    // The short form stays short: a plain version reads better than a one-key
    // inline table, and most dependencies need nothing more.
    let value = if dep.features.is_empty() && dep.default_features {
        Value::from(dep.version.as_str())
    } else {
        let mut detail = InlineTable::new();
        detail.insert("version", Value::from(dep.version.as_str()));
        if !dep.features.is_empty() {
            let mut features = Array::new();
            for f in &dep.features {
                features.push(f.as_str());
            }
            detail.insert("features", Value::Array(features));
        }
        if !dep.default_features {
            detail.insert("default-features", Value::from(false));
        }
        Value::InlineTable(detail)
    };

    table.insert(&dep.name, Item::Value(value));
    write(dir, &doc)
}

/// Removes a dependency.
///
/// Including `wit-bindgen`, without which the crate stops being a component.
/// Removing it breaks the build, the compile report says so plainly, and a
/// rollback undoes it - which teaches more than a refusal does.
pub fn remove(dir: &Path, name: &str) -> Result<()> {
    validate_name(name)?;

    let mut doc = read(dir)?;
    let removed = doc
        .get_mut("dependencies")
        .and_then(Item::as_table_like_mut)
        .and_then(|t| t.remove(name))
        .is_some();

    if !removed {
        bail!("{name} is not a dependency of this crate");
    }
    write(dir, &doc)
}

fn read(dir: &Path) -> Result<DocumentMut> {
    let path = dir.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))
}

fn write(dir: &Path, doc: &DocumentMut) -> Result<()> {
    let path = dir.join("Cargo.toml");
    std::fs::write(&path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

/// Crate names as crates.io accepts them.
///
/// This is also what stops a name being used to inject TOML: nothing that
/// passes here can close a quote or open a new table.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME {
        bail!("{name:?} is not a usable crate name");
    }
    if !name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        bail!("crate names start with a letter or digit: {name:?}");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("{name:?} contains characters that are not valid in a crate name");
    }
    Ok(())
}

/// A cargo version requirement, e.g. `1`, `0.4.31`, `^1.2`, `>=1, <3`.
fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > MAX_VERSION {
        bail!("{version:?} is not a usable version requirement");
    }
    let ok = version.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '.' | '*' | '^' | '~' | '<' | '>' | '=' | ',' | ' ' | '-' | '+'
            )
    });
    if !ok {
        bail!("{version:?} is not a valid version requirement");
    }
    Ok(())
}

fn validate_feature(feature: &str) -> Result<()> {
    if feature.is_empty() || feature.len() > MAX_NAME {
        bail!("{feature:?} is not a usable feature name");
    }
    if !feature
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
    {
        bail!("{feature:?} contains characters that are not valid in a feature name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"[package]
name = "probe"
version = "0.1.0"
edition = "2021"

# Standalone: built for wasm32-wasip2 by the orchestrator.
[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.60"
serde_json = "1"

[profile.release]
opt-level = "s"
"#;

    fn scratch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), MANIFEST).unwrap();
        dir
    }

    fn dep(name: &str) -> Dependency {
        Dependency {
            name: name.to_string(),
            version: "1".to_string(),
            features: Vec::new(),
            default_features: true,
        }
    }

    #[test]
    fn adds_a_plain_dependency_and_leaves_everything_else_alone() {
        let dir = scratch();
        add(dir.path(), &dep("regex"), &[]).unwrap();

        let text = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(text.contains(r#"regex = "1""#));
        // The parts that make this crate a component are untouched.
        assert!(text.contains(r#"crate-type = ["cdylib"]"#));
        assert!(text.contains("[workspace]"));
        assert!(text.contains("# Standalone: built for wasm32-wasip2 by the orchestrator."));
        assert!(text.contains(r#"opt-level = "s""#));
    }

    #[test]
    fn writes_the_detailed_form_only_when_it_is_needed() {
        let dir = scratch();
        add(
            dir.path(),
            &Dependency {
                name: "tokio".into(),
                version: "1".into(),
                features: vec!["rt".into(), "macros".into()],
                default_features: false,
            },
            &[],
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(text.contains(r#"features = ["rt", "macros"]"#), "{text}");
        assert!(text.contains("default-features = false"));
    }

    #[test]
    fn adding_an_existing_dependency_replaces_it() {
        let dir = scratch();
        add(dir.path(), &dep("serde_json"), &[]).unwrap();
        let found = list(dir.path()).unwrap();
        assert_eq!(found.iter().filter(|d| d.name == "serde_json").count(), 1);
    }

    #[test]
    fn lists_both_manifest_forms() {
        let dir = scratch();
        add(
            dir.path(),
            &Dependency {
                name: "image".into(),
                version: "0.25".into(),
                features: vec!["png".into()],
                default_features: false,
            },
            &[],
        )
        .unwrap();

        let found = list(dir.path()).unwrap();
        let plain = found.iter().find(|d| d.name == "serde_json").unwrap();
        assert_eq!(plain.version, "1");
        assert!(plain.default_features);

        let detailed = found.iter().find(|d| d.name == "image").unwrap();
        assert_eq!(detailed.version, "0.25");
        assert_eq!(detailed.features, vec!["png".to_string()]);
        assert!(!detailed.default_features);
    }

    #[test]
    fn removes_any_dependency_including_wit_bindgen() {
        let dir = scratch();
        remove(dir.path(), "serde_json").unwrap();
        assert!(!list(dir.path())
            .unwrap()
            .iter()
            .any(|d| d.name == "serde_json"));

        // The crate stops being a component without it. That shows up as a
        // failed build with the reason attached, not as a refusal here.
        remove(dir.path(), "wit-bindgen").unwrap();
        assert!(!list(dir.path())
            .unwrap()
            .iter()
            .any(|d| d.name == "wit-bindgen"));
    }

    #[test]
    fn removing_something_absent_says_so() {
        let dir = scratch();
        let err = remove(dir.path(), "nope").unwrap_err().to_string();
        assert!(err.contains("not a dependency"), "{err}");
    }

    #[test]
    fn rejects_names_that_could_inject_toml() {
        let dir = scratch();
        for bad in [
            "serde\"\n[package]\nname = \"evil",
            "../../etc/passwd",
            "has space",
            "-leading-dash",
            "",
        ] {
            assert!(
                add(dir.path(), &dep(bad), &[]).is_err(),
                "should have rejected {bad:?}"
            );
        }
        // The manifest survived every attempt.
        let text = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert_eq!(text, MANIFEST);
    }

    #[test]
    fn rejects_malformed_versions_and_features() {
        let dir = scratch();
        let mut bad_version = dep("regex");
        bad_version.version = "1\"\nevil = \"".into();
        assert!(add(dir.path(), &bad_version, &[]).is_err());

        let mut bad_feature = dep("regex");
        bad_feature.features = vec!["ok".into(), "not ok!".into()];
        assert!(add(dir.path(), &bad_feature, &[]).is_err());
    }

    #[test]
    fn honours_the_allow_list_when_one_is_set() {
        let dir = scratch();
        let allowed = vec!["regex".to_string()];
        assert!(add(dir.path(), &dep("regex"), &allowed).is_ok());

        let err = add(dir.path(), &dep("image"), &allowed)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the allowed crate list"), "{err}");
    }

    #[test]
    fn accepts_the_version_requirements_cargo_accepts() {
        for good in ["1", "0.4.31", "^1.2", "~0.3", ">=1, <3", "1.0.0-alpha.1", "*"] {
            assert!(validate_version(good).is_ok(), "rejected {good}");
        }
    }
}
