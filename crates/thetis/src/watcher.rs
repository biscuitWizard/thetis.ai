//! Hot reload.
//!
//! Watches the guest source trees and pushes anything that changes through the
//! same pipeline the agent's self-modification uses. Editing a file is
//! therefore exactly as safe as the agent editing itself: a broken edit is
//! caught by the gates and the running system is untouched.

use anyhow::{Context, Result};
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;


use crate::config::Config;
use crate::grip::Grip;
use crate::pipeline;
use crate::revisions::Origin;
use crate::aspect::Aspect;

pub type WatchHandle = Debouncer<notify::RecommendedWatcher, RecommendedCache>;

/// Starts watching. The returned handle must be kept alive: dropping it stops
/// the watcher.
pub fn spawn(grip: Arc<Grip>) -> Result<WatchHandle> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Aspect>();
    let cfg = grip.cfg.clone();

    let debounce = grip.cfg.watchdog.debounce;
    let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
        let Ok(events) = result else { return };
        let mut aspects = HashSet::new();
        for event in events {
            if !is_source_change(&event.kind) {
                continue;
            }
            for path in &event.paths {
                aspects.extend(aspects_for_path(&cfg, path));
            }
        }
        for aspect in aspects {
            let _ = tx.send(aspect);
        }
    })
    .context("creating file watcher")?;

    for path in grip.cfg.watched_dirs() {
        if path.is_dir() {
            debouncer
                .watch(&path, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", path.display()))?;
            tracing::debug!(dir = %path.display(), "watching for changes");
        }
    }

    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            // Coalesce everything already queued so a multi-file save triggers
            // one build per aspect, not one per file.
            let mut pending: HashSet<Aspect> = HashSet::from([first]);
            while let Ok(more) = rx.try_recv() {
                pending.insert(more);
            }

            for aspect in pending {
                rebuild(&grip, &aspect).await;
            }
        }
    });

    Ok(debouncer)
}

async fn rebuild(grip: &Arc<Grip>, aspect: &Aspect) {
    if grip.watch_suppressed(aspect) {
        tracing::debug!(%aspect, "ignoring change: the orchestrator wrote this itself");
        return;
    }
    // Who changed it? A write through the devkit is suppressed above, so
    // anything arriving here came from outside the orchestrator: a person in an
    // editor, or the agent driving a shell. On disk those are identical, and a
    // turn in flight is the one signal that separates them — read now, before
    // a build of our own makes this process look busy for another reason.
    let origin = if grip.turn_in_flight() {
        Origin::AgentMod
    } else {
        Origin::HumanEdit
    };

    tracing::info!(%aspect, "source changed, rebuilding");

    // The note says what is true of this build. It deliberately does not claim
    // the commit holds only this aspect: a checkpoint sweeps the whole worktree,
    // so the file that changed and the aspect that rebuilt need not match.
    match pipeline::build_and_activate(grip, aspect, origin, "rebuilt after a change on disk")
        .await
    {
        Ok(outcome) if outcome.success => {
            tracing::info!(
                %aspect,
                revision = outcome.revision.unwrap_or(0),
                took_ms = outcome.duration_ms,
                "hot swapped"
            );
        }
        Ok(outcome) => {
            // Deliberately not fatal: the previous revision is still serving.
            tracing::warn!(%aspect, detail = %outcome.detail, "rebuild rejected");
            if !outcome.stderr.is_empty() {
                tracing::warn!(%aspect, "\n{}", outcome.stderr);
            }
        }
        Err(e) => tracing::error!(%aspect, error = %e, "rebuild pipeline failed"),
    }
}

/// Whether an event actually changed source, as opposed to merely touching it.
///
/// This is load-bearing. Linux reports reads: cargo opening `Cargo.toml` and
/// every `.rs` file during a build emits `Access(Open)` for each one. Treating
/// those as source changes makes a build its own trigger, and the watcher spins
/// rebuilding forever at whatever rate the debounce allows. Windows has no
/// file-open notification at all, so the loop never appears there.
///
/// Timestamp-only changes are excluded for the same reason: restoring a
/// revision touches files so cargo notices them, and that must not read back as
/// a fresh edit.
fn is_source_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => true,
        // Platforms that cannot classify an event report `Any`; assume it counted.
        EventKind::Any | EventKind::Other => true,
    }
}

/// Which aspects a changed path affects.
///
/// A change under `wit/` alters the contract every guest is compiled against,
/// so it rebuilds all of them.
fn aspects_for_path(cfg: &Config, path: &Path) -> Vec<Aspect> {
    // Build output and version control churn are not source changes.
    if path.components().any(|c| {
        let part = c.as_os_str().to_string_lossy();
        part == "target" || part == ".git" || part.ends_with(".lock")
    }) {
        return Vec::new();
    }

    // A change to the contract recompiles every guest against it.
    if path.starts_with(&cfg.paths.wit) {
        return all_source_aspects(cfg);
    }
    if path.starts_with(&cfg.paths.agent) {
        return vec![Aspect::Agent];
    }

    for (root, prefix, make) in source_roots(cfg) {
        if let Ok(relative) = path.strip_prefix(root) {
            let Some(dir) = relative.components().next() else {
                continue;
            };
            let name = dir.as_os_str().to_string_lossy();
            if let Some(short) = name.strip_prefix(prefix.as_str()) {
                return vec![make(short)];
            }
        }
    }

    Vec::new()
}

/// Where gateways and tools live, with the naming convention each follows.
fn source_roots(cfg: &Config) -> [(&Path, &String, fn(&str) -> Aspect); 2] {
    // Concrete wrappers: the generic constructors cannot coerce to a fn pointer.
    fn gateway(name: &str) -> Aspect {
        Aspect::Gateway(name.to_string())
    }
    fn tool(name: &str) -> Aspect {
        Aspect::Tool(name.to_string())
    }

    [
        (cfg.paths.gateways.as_path(), &cfg.paths.gateway_prefix, gateway),
        (cfg.paths.tools.as_path(), &cfg.paths.tool_prefix, tool),
    ]
}

/// Every aspect that currently has a source tree on disk.
fn all_source_aspects(cfg: &Config) -> Vec<Aspect> {
    let mut aspects = Vec::new();
    if cfg.paths.agent.join("Cargo.toml").is_file() {
        aspects.push(Aspect::Agent);
    }

    for (root, prefix, make) in source_roots(cfg) {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().join("Cargo.toml").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(short) = name.strip_prefix(prefix.as_str()) {
                aspects.push(make(short));
            }
        }
    }
    aspects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> Config {
        let root = Path::new("C:/proj");
        let mut cfg = Config::load().unwrap();
        cfg.root = root.to_path_buf();
        cfg.paths.wit = root.join("wit");
        cfg.paths.agent = root.join("agents/agent-core");
        cfg.paths.gateways = root.join("gateways");
        cfg.paths.tools = root.join("tools");
        cfg
    }

    #[test]
    fn maps_source_paths_to_their_aspects() {
        let cfg = test_cfg();
        assert_eq!(
            aspects_for_path(&cfg, Path::new("C:/proj/agents/agent-core/src/lib.rs")),
            vec![Aspect::Agent]
        );
        assert_eq!(
            aspects_for_path(&cfg, Path::new("C:/proj/gateways/gateway-web/src/ui/app.js")),
            vec![Aspect::gateway("web")]
        );
        assert_eq!(
            aspects_for_path(&cfg, Path::new("C:/proj/tools/weather/src/lib.rs")),
            vec![Aspect::tool("weather")]
        );
    }

    #[test]
    fn ignores_reads_so_a_build_cannot_retrigger_itself() {
        use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, MetadataKind,
                            RemoveKind};

        // What cargo generates just by reading the sources it compiles.
        assert!(!is_source_change(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!is_source_change(&EventKind::Access(AccessKind::Read)));
        // A restore touches files; that is not an edit.
        assert!(!is_source_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::WriteTime
        ))));

        // Real edits still rebuild.
        assert!(is_source_change(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
        assert!(is_source_change(&EventKind::Create(CreateKind::File)));
        assert!(is_source_change(&EventKind::Remove(RemoveKind::File)));
        assert!(is_source_change(&EventKind::Any));
    }

    #[test]
    fn ignores_build_output_and_unrelated_paths() {
        let cfg = test_cfg();
        for path in [
            "C:/proj/target-wasm/wasm32-wasip2/release/agent_core.wasm",
            "C:/proj/agents/agent-core/target/debug/x.rlib",
            "C:/proj/data/thetis.redb",
            "C:/elsewhere/agents/agent-core/src/lib.rs",
        ] {
            assert!(
                aspects_for_path(&cfg, Path::new(path)).is_empty(),
                "should have ignored {path}"
            );
        }
    }
}
