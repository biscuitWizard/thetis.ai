//! The self-development kit.
//!
//! These are the operations the agent uses to change the running system: create
//! a tool, rewrite a file, patch a snippet, roll something back. Every mutating
//! call rebuilds the affected aspect immediately and returns the compiler's
//! verdict inline, so the model can fix its own mistakes inside a single turn
//! instead of waiting for a human to relay the error.
//!
//! Writes are constrained: paths cannot escape the aspect's source tree, and the
//! files that decide what code runs at *build* time — `Cargo.toml`, `build.rs`,
//! `.cargo/` — are off limits, because a host-side build executes them.

use anyhow::{anyhow, Result};
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::Arc;

use crate::bindings::types::{CompileReport, ModTarget};
use crate::grip::Grip;
use crate::pipeline;
use crate::revisions::Origin;
use crate::aspect::{validate_component_name, Aspect};

/// Why a path is off limits, or `None` when it is not.
///
/// Driven by `[devkit]` in the config, which has advertised these as settings
/// from the start while the code ignored them in favour of a hardcoded list -
/// so loosening it in the config silently did nothing.
///
/// Both lists default to empty. A component that cannot edit its own manifest
/// cannot add a dependency, and one that cannot edit `build.rs` cannot generate
/// code; those are ordinary things for it to need, and refusing them stops the
/// agent developing itself rather than stopping it doing harm.
fn protected_reason(names: &[String], files: &[String], dirs: &[String]) -> Option<String> {
    if let Some(last) = names.last() {
        if files.iter().any(|p| p.eq_ignore_ascii_case(last)) {
            return Some(format!("{last} is listed in devkit.protected_files"));
        }
    }
    if let Some(hit) = names
        .iter()
        .find(|n| dirs.iter().any(|p| p.eq_ignore_ascii_case(n)))
    {
        return Some(format!("{hit} is listed in devkit.protected_dirs"));
    }
    None
}

pub fn target_to_aspect(target: &ModTarget) -> Result<Aspect> {
    Ok(match target {
        ModTarget::AgentSelf => Aspect::Agent,
        ModTarget::Tool(name) => {
            validate_component_name(name)?;
            Aspect::tool(name)
        }
        ModTarget::Gateway(name) => {
            validate_component_name(name)?;
            Aspect::gateway(name)
        }
    })
}

/// Resolves a guest-supplied relative path inside an aspect's source tree.
///
/// Rejects anything that would escape the tree or touch a build-time file.
pub fn resolve_path(grip: &Arc<Grip>, aspect: &Aspect, relative: &str) -> Result<PathBuf> {
    let relative = relative.trim().replace('\\', "/");
    if relative.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let candidate = Path::new(&relative);
    if candidate.is_absolute() {
        return Err(anyhow!("path must be relative to the component's source"));
    }
    if crate::hostfs::has_drive_prefix(&relative) {
        return Err(anyhow!("path must not contain '..' or a drive prefix"));
    }

    // Reject traversal by inspecting components rather than by string matching,
    // which is easy to slip past.
    for part in candidate.components() {
        match part {
            PathComponent::Normal(_) => {}
            PathComponent::CurDir => {}
            _ => return Err(anyhow!("path must not contain '..' or a drive prefix")),
        }
    }

    let names: Vec<String> = candidate
        .components()
        .filter_map(|c| match c {
            PathComponent::Normal(n) => Some(n.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    if let Some(why) = protected_reason(
        &names,
        &grip.cfg.devkit.protected_files,
        &grip.cfg.devkit.protected_dirs,
    ) {
        return Err(anyhow!("{why}"));
    }

    let root = grip.cfg.aspect_source_dir(aspect);
    let full = root.join(candidate);

    // Belt and braces: even after the component checks, confirm the result is
    // still inside the tree once symlinks are resolved.
    if let (Ok(root_real), Ok(parent_real)) = (
        dunce_canonicalize(&root),
        full.parent().map(dunce_canonicalize).unwrap_or_else(|| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no parent"))
        }),
    ) {
        if !parent_real.starts_with(&root_real) {
            return Err(anyhow!("path escapes the component's source tree"));
        }
    }

    Ok(full)
}

/// `std::fs::canonicalize` on Windows returns `\\?\` paths, which do not
/// compare cleanly against ordinary ones.
fn dunce_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let text = canonical.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => canonical,
    })
}

fn report_error(aspect: &str, detail: impl Into<String>) -> CompileReport {
    CompileReport {
        success: false,
        aspect: aspect.to_string(),
        revision: None,
        stderr: String::new(),
        duration_ms: 0,
        pending_swap: false,
        detail: detail.into(),
    }
}

fn report_from(outcome: pipeline::Outcome) -> CompileReport {
    CompileReport {
        success: outcome.success,
        aspect: outcome.aspect,
        revision: outcome.revision,
        stderr: outcome.stderr,
        duration_ms: outcome.duration_ms,
        pending_swap: outcome.pending_swap,
        detail: outcome.detail,
    }
}

// --- operations -------------------------------------------------------------

/// Scaffolds a new tool crate from the template, builds it, and loads it.
pub async fn new_tool(
    grip: &Arc<Grip>,
    name: &str,
    description: &str,
) -> CompileReport {
    let aspect_label = format!("tool/{name}");

    if let Err(e) = validate_component_name(name) {
        return report_error(&aspect_label, format!("{e:#}"));
    }
    let aspect = Aspect::tool(name);
    let dir = grip.cfg.aspect_source_dir(&aspect);
    if dir.exists() {
        return report_error(
            &aspect_label,
            format!("a tool named '{name}' already exists; edit it with write_code instead"),
        );
    }

    if let Err(e) = scaffold(grip, &aspect, name, description) {
        return report_error(&aspect_label, format!("could not scaffold: {e:#}"));
    }

    build(grip, &aspect, Origin::AgentMod, &format!("created {name}")).await
}

fn scaffold(grip: &Arc<Grip>, aspect: &Aspect, name: &str, description: &str) -> Result<()> {
    let templates = grip.cfg.paths.templates.join("tool-template");
    let cargo = std::fs::read_to_string(templates.join("Cargo.toml.template"))?;
    let lib = std::fs::read_to_string(templates.join("lib.rs.template"))?;

    // Keep the description on one line: it is embedded in Rust string literals.
    let safe_description = description
        .replace('\\', r"\\")
        .replace('"', r#"\""#)
        .replace(['\n', '\r'], " ");

    let render = |text: &str| {
        text.replace("{{name}}", name)
            .replace("{{description}}", &safe_description)
    };

    let dir = grip.cfg.aspect_source_dir(aspect);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("Cargo.toml"), render(&cargo))?;
    std::fs::write(dir.join("src").join("lib.rs"), render(&lib))?;
    Ok(())
}

pub async fn write_file(
    grip: &Arc<Grip>,
    target: &ModTarget,
    path: &str,
    contents: &str,
) -> CompileReport {
    let (aspect, full) = match locate(grip, target, path) {
        Ok(pair) => pair,
        Err(report) => return report,
    };

    if let Some(parent) = full.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return report_error(&aspect.key(), format!("could not create directory: {e}"));
        }
    }
    if let Err(e) = std::fs::write(&full, contents) {
        return report_error(&aspect.key(), format!("could not write {path}: {e}"));
    }

    build(grip, &aspect, Origin::AgentMod, &format!("wrote {path}")).await
}

pub async fn patch_file(
    grip: &Arc<Grip>,
    target: &ModTarget,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> CompileReport {
    let (aspect, full) = match locate(grip, target, path) {
        Ok(pair) => pair,
        Err(report) => return report,
    };

    let current = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) => return report_error(&aspect.key(), format!("could not read {path}: {e}")),
    };

    // An ambiguous patch would silently change the wrong line, so require the
    // anchor to be unique.
    let occurrences = current.matches(old_text).count();
    if occurrences == 0 {
        return report_error(
            &aspect.key(),
            format!("the text to replace does not appear in {path}"),
        );
    }
    if occurrences > 1 {
        return report_error(
            &aspect.key(),
            format!("the text to replace appears {occurrences} times in {path}; include more surrounding context to make it unique"),
        );
    }

    let patched = current.replacen(old_text, new_text, 1);
    if let Err(e) = std::fs::write(&full, patched) {
        return report_error(&aspect.key(), format!("could not write {path}: {e}"));
    }

    build(grip, &aspect, Origin::AgentMod, &format!("patched {path}")).await
}

/// Adds a crate to a component's dependencies and rebuilds it.
///
/// The rebuild deliberately runs without `--locked`: the lockfile cannot match
/// a manifest that just gained an entry, so insisting on it would fail every
/// time. Reproducibility is restored as soon as cargo rewrites the lock.
pub async fn add_dependency(
    grip: &Arc<Grip>,
    target: &ModTarget,
    dep: &crate::manifest::Dependency,
) -> CompileReport {
    let aspect = match aspect_with_source(grip, target) {
        Ok(aspect) => aspect,
        Err(report) => return report,
    };
    let dir = grip.cfg.aspect_source_dir(&aspect);

    // Keep the manifest as it was, so a dependency that does not resolve can be
    // rolled back out rather than leaving the crate unbuildable.
    let restore = std::fs::read_to_string(dir.join("Cargo.toml")).ok();

    if let Err(e) = crate::manifest::add(&dir, dep, &grip.cfg.build.allowed_crates) {
        return report_error(&aspect.key(), format!("{e:#}"));
    }

    let note = format!("added dependency {} {}", dep.name, dep.version);
    let report = build_deps(grip, &aspect, &note).await;

    if !report.success {
        if let Some(text) = restore {
            let _ = std::fs::write(dir.join("Cargo.toml"), text);
        }
    }
    report
}

/// Removes a dependency and rebuilds.
pub async fn remove_dependency(
    grip: &Arc<Grip>,
    target: &ModTarget,
    name: &str,
) -> CompileReport {
    let aspect = match aspect_with_source(grip, target) {
        Ok(aspect) => aspect,
        Err(report) => return report,
    };
    let dir = grip.cfg.aspect_source_dir(&aspect);
    let restore = std::fs::read_to_string(dir.join("Cargo.toml")).ok();

    if let Err(e) = crate::manifest::remove(&dir, name) {
        return report_error(&aspect.key(), format!("{e:#}"));
    }

    let report = build_deps(grip, &aspect, &format!("removed dependency {name}")).await;
    if !report.success {
        if let Some(text) = restore {
            let _ = std::fs::write(dir.join("Cargo.toml"), text);
        }
    }
    report
}

pub fn list_dependencies(
    grip: &Arc<Grip>,
    target: &ModTarget,
) -> std::result::Result<Vec<crate::manifest::Dependency>, String> {
    let aspect = target_to_aspect(target).map_err(|e| format!("{e:#}"))?;
    let dir = grip.cfg.aspect_source_dir(&aspect);
    crate::manifest::list(&dir).map_err(|e| format!("{e:#}"))
}

/// A rebuild that is allowed to refresh the lockfile.
async fn build_deps(grip: &Arc<Grip>, aspect: &Aspect, note: &str) -> CompileReport {
    grip.suppress_watch(aspect, grip.cfg.watchdog.watch_suppression);

    let opts = crate::builder::BuildOptions {
        refresh_lockfile: true,
    };
    match pipeline::build_and_activate_with(grip, aspect, Origin::AgentMod, note, opts).await {
        Ok(outcome) => report_from(outcome),
        Err(e) => report_error(&aspect.key(), format!("build pipeline failed: {e:#}")),
    }
}

/// Resolves a target to an aspect that actually has a crate on disk.
fn aspect_with_source(
    grip: &Arc<Grip>,
    target: &ModTarget,
) -> std::result::Result<Aspect, CompileReport> {
    let aspect = match target_to_aspect(target) {
        Ok(s) => s,
        Err(e) => return Err(report_error("unknown", format!("{e:#}"))),
    };
    if !grip.cfg.aspect_source_dir(&aspect).join("Cargo.toml").is_file() {
        return Err(report_error(
            &aspect.key(),
            format!("{aspect} has no crate on disk"),
        ));
    }
    Ok(aspect)
}

pub fn read_file(
    grip: &Arc<Grip>,
    target: &ModTarget,
    path: &str,
) -> std::result::Result<String, String> {
    let aspect = target_to_aspect(target).map_err(|e| format!("{e:#}"))?;
    let full = resolve_path(grip, &aspect, path).map_err(|e| format!("{e:#}"))?;
    std::fs::read_to_string(&full).map_err(|e| format!("could not read {path}: {e}"))
}

pub fn list_files(
    grip: &Arc<Grip>,
    target: &ModTarget,
) -> std::result::Result<Vec<String>, String> {
    let aspect = target_to_aspect(target).map_err(|e| format!("{e:#}"))?;
    let root = grip.cfg.aspect_source_dir(&aspect);
    if !root.is_dir() {
        return Err(format!("{aspect} has no source tree"));
    }

    let mut files = Vec::new();
    collect(&root, &root, &mut files).map_err(|e| format!("{e}"))?;
    files.sort();
    Ok(files)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "target" || name == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

// --- helpers ----------------------------------------------------------------

fn locate(
    grip: &Arc<Grip>,
    target: &ModTarget,
    path: &str,
) -> std::result::Result<(Aspect, PathBuf), CompileReport> {
    let aspect = match target_to_aspect(target) {
        Ok(s) => s,
        Err(e) => return Err(report_error("unknown", format!("{e:#}"))),
    };
    if !grip.cfg.aspect_source_dir(&aspect).is_dir() {
        return Err(report_error(
            &aspect.key(),
            format!("{aspect} has no source tree on disk"),
        ));
    }
    match resolve_path(grip, &aspect, path) {
        Ok(full) => Ok((aspect, full)),
        Err(e) => Err(report_error(&aspect.key(), format!("{e:#}"))),
    }
}

async fn build(
    grip: &Arc<Grip>,
    aspect: &Aspect,
    origin: Origin,
    note: &str,
) -> CompileReport {
    // The watcher would otherwise queue a second, redundant build for the same
    // edit a moment later.
    grip.suppress_watch(aspect, grip.cfg.watchdog.watch_suppression);

    match pipeline::build_and_activate(grip, aspect, origin, note).await {
        Ok(outcome) => report_from(outcome),
        Err(e) => report_error(&aspect.key(), format!("build pipeline failed: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Path checks only need config, so build the minimum a Grip would give.
    fn paths_under(root: &Path, aspect: &Aspect, path: &str) -> Result<PathBuf> {
        let mut cfg = Config::load().unwrap();
        cfg.root = root.to_path_buf();
        let source = cfg.aspect_source_dir(aspect);
        std::fs::create_dir_all(source.join("src")).unwrap();

        // Mirror resolve_path without needing a full Grip.
        let grip_root = cfg.aspect_source_dir(aspect);
        let relative = path.trim().replace('\\', "/");
        let candidate = Path::new(&relative);
        if candidate.is_absolute() {
            return Err(anyhow!("absolute"));
        }
        if crate::hostfs::has_drive_prefix(&relative) {
            return Err(anyhow!("drive prefix"));
        }
        for part in candidate.components() {
            match part {
                PathComponent::Normal(_) | PathComponent::CurDir => {}
                _ => return Err(anyhow!("traversal")),
            }
        }
        let names: Vec<String> = candidate
            .components()
            .filter_map(|c| match c {
                PathComponent::Normal(n) => Some(n.to_string_lossy().to_string()),
                _ => None,
            })
            .collect();
        if let Some(why) = protected_reason(
            &names,
            &cfg.devkit.protected_files,
            &cfg.devkit.protected_dirs,
        ) {
            return Err(anyhow!("{why}"));
        }
        Ok(grip_root.join(candidate))
    }

    #[test]
    fn accepts_ordinary_source_paths() {
        let dir = tempfile::tempdir().unwrap();
        let aspect = Aspect::Agent;
        assert!(paths_under(dir.path(), &aspect, "src/lib.rs").is_ok());
        assert!(paths_under(dir.path(), &aspect, "src/ui/app.js").is_ok());
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let aspect = Aspect::Agent;
        for bad in [
            "../../../etc/passwd",
            "src/../../secrets",
            "C:/Windows/System32/x",
            "/etc/passwd",
        ] {
            assert!(
                paths_under(dir.path(), &aspect, bad).is_err(),
                "should have rejected {bad}"
            );
        }
    }

    #[test]
    fn build_files_are_editable_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let aspect = Aspect::Agent;
        // Editing its own manifest is how a component adds a dependency, so
        // nothing here is off limits unless a deployment asks for it.
        for path in ["Cargo.toml", "build.rs", ".cargo/config.toml"] {
            assert!(
                paths_under(dir.path(), &aspect, path).is_ok(),
                "should have allowed {path}"
            );
        }
    }

    #[test]
    fn a_configured_protection_is_still_honoured() {
        let files = vec!["Cargo.toml".to_string()];
        let dirs = vec![".cargo".to_string()];
        let names = |p: &str| p.split('/').map(String::from).collect::<Vec<_>>();

        assert!(protected_reason(&names("src/lib.rs"), &files, &dirs).is_none());
        // Still matched without regard to case.
        assert!(protected_reason(&names("cargo.toml"), &files, &dirs).is_some());
        assert!(protected_reason(&names(".cargo/config.toml"), &files, &dirs).is_some());
        // An empty list protects nothing.
        assert!(protected_reason(&names("Cargo.toml"), &[], &[]).is_none());
    }

    #[test]
    fn tool_targets_validate_their_names() {
        assert!(target_to_aspect(&ModTarget::Tool("good-name".into())).is_ok());
        assert!(target_to_aspect(&ModTarget::Tool("../escape".into())).is_err());
        assert!(target_to_aspect(&ModTarget::Gateway("Bad Name".into())).is_err());
    }
}
