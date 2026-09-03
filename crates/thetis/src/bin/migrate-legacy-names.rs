//! One-time migration for the rename that made guests *aspects*.
//!
//! Two things on disk carry the old field name and would fail to decode
//! against the new types:
//!
//! * the revision registry in redb, whose rows are `{"slot": ...}`;
//! * every build-cache `meta.json`, whose entries carry `slot` and `slot_tree`.
//!
//! Nothing else does. The event log is untouched (its records never carried
//! the field), redb *keys* are untouched (an aspect's key string — "agent",
//! "gateway/web", "tool/x" — did not change), and the snapshots table is
//! unused.
//!
//! Decoding is strict on the read path (one bad row fails the whole query), so
//! this is not optional if the old data is being kept: without it the revision
//! panel and every rollback would error, and every green build verdict would
//! be invisible to the merge gate.
//!
//! Usage:
//!
//! ```text
//! migrate-legacy-names <data-dir> <artifacts-dir> [--apply]
//! ```
//!
//! Without `--apply` it only reports what it would change. Take a copy of the
//! database first; this rewrites in place.

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::Value;
use std::path::{Path, PathBuf};

const REVISIONS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("revisions");
const BRANCHES: TableDefinition<&str, &[u8]> = TableDefinition::new("branches");

/// Old field name -> new field name.
const RENAMES: &[(&str, &str)] = &[("slot", "aspect"), ("slot_tree", "aspect_tree")];

/// Renames the keys of one JSON object in place. Returns whether it changed.
fn rename_keys(value: &mut Value) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    let mut touched = false;
    for (from, to) in RENAMES {
        // Only when the new name is not already there, so re-running is safe.
        if object.contains_key(*from) && !object.contains_key(*to) {
            if let Some(v) = object.remove(*from) {
                object.insert((*to).to_string(), v);
                touched = true;
            }
        }
    }
    touched
}

fn migrate_revisions(db_path: &Path, apply: bool) -> Result<usize> {
    if !db_path.is_file() {
        println!("no database at {} — nothing to migrate", db_path.display());
        return Ok(0);
    }
    let db = Database::open(db_path)
        .with_context(|| format!("opening {} (is anything still running?)", db_path.display()))?;

    // Read every row first; the write transaction is taken only once, and only
    // if there is something to do.
    let mut pending: Vec<((String, u64), Vec<u8>)> = Vec::new();
    {
        let txn = db.begin_read()?;
        let table = match txn.open_table(REVISIONS) {
            Ok(table) => table,
            // A database that never held a revision has no such table.
            Err(_) => return Ok(0),
        };
        for row in table.iter()? {
            let (k, v) = row?;
            let (aspect_key, revision) = k.value();
            let mut parsed: Value = serde_json::from_slice(v.value())
                .with_context(|| format!("decoding revision {aspect_key}#{revision}"))?;
            if rename_keys(&mut parsed) {
                pending.push((
                    (aspect_key.to_string(), revision),
                    serde_json::to_vec(&parsed)?,
                ));
            }
        }
    }

    if pending.is_empty() || !apply {
        return Ok(pending.len());
    }

    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(REVISIONS)?;
        for ((aspect_key, revision), bytes) in &pending {
            table.insert((aspect_key.as_str(), *revision), bytes.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(pending.len())
}

fn migrate_build_cache(cache_root: &Path, apply: bool) -> Result<usize> {
    let mut changed = 0;
    let mut stack: Vec<PathBuf> = vec![cache_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("meta.json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let mut parsed: Value = match serde_json::from_str(&text) {
                Ok(parsed) => parsed,
                // A half-written entry is the cache's problem, not ours.
                Err(e) => {
                    eprintln!("skipping {}: {e}", path.display());
                    continue;
                }
            };
            if rename_keys(&mut parsed) {
                changed += 1;
                if apply {
                    std::fs::write(&path, serde_json::to_vec_pretty(&parsed)?)?;
                }
            }
        }
    }
    Ok(changed)
}

/// Reads everything back through the *real* types.
///
/// The rename above is a key rewrite, which is easy to get subtly wrong; this
/// is the part that actually answers the question the operator cares about —
/// does the data decode now. It is why the migration is worth running before
/// the switchover rather than discovering it on the first page load.
fn verify(db_path: &Path, cache_root: &Path) -> Result<(usize, usize)> {
    let mut rows = 0;
    if db_path.is_file() {
        let db = Database::open(db_path)?;
        let txn = db.begin_read()?;
        if let Ok(table) = txn.open_table(REVISIONS) {
            for row in table.iter()? {
                let (k, v) = row?;
                let (aspect_key, revision) = k.value();
                serde_json::from_slice::<thetis::revisions::RevisionRow>(v.value())
                    .with_context(|| format!("revision {aspect_key}#{revision} still does not decode"))?;
                rows += 1;
            }
        }
    }

    let mut entries = 0;
    let mut stack: Vec<PathBuf> = vec![cache_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("meta.json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            serde_json::from_str::<thetis::buildcache::BuildMeta>(&text)
                .with_context(|| format!("{} still does not decode", path.display()))?;
            entries += 1;
        }
    }
    Ok((rows, entries))
}

/// Re-points every conversation branch at the current trunk commit.
///
/// A conversation row pins the trunk commit its branch forked from, and the
/// commit of any kernel it built for itself. Rewriting trunk's history leaves
/// both naming commits that trunk no longer has: the branch panel then shows a
/// base that does not exist, and the worker looks for a cached kernel under a
/// path that was never written. Re-pointing the base and clearing the kernel
/// (empty means "run the trunk binary") puts both back in step.
///
/// Only sensible immediately after the branches themselves have been rebased
/// onto that commit.
fn repoint_branches(db_path: &Path, base: &str, root: &Path, apply: bool) -> Result<usize> {
    if !db_path.is_file() {
        return Ok(0);
    }
    let db = Database::open(db_path)?;
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    {
        let txn = db.begin_read()?;
        let table = match txn.open_table(BRANCHES) {
            Ok(table) => table,
            Err(_) => return Ok(0),
        };
        for row in table.iter()? {
            let (k, v) = row?;
            let session = k.value().to_string();
            let mut parsed: Value = serde_json::from_slice(v.value())
                .with_context(|| format!("decoding the branch row for {session}"))?;
            let Some(object) = parsed.as_object_mut() else {
                continue;
            };
            // The checkout path is absolute and stored, so moving the tree
            // leaves it naming a directory that is gone. The worker then finds
            // no checkout there and tries to create one — which fails, because
            // the branch is already checked out at the *new* path, and the
            // conversation cannot be opened at all.
            let wanted_worktree = object
                .get("worktree")
                .and_then(Value::as_str)
                .and_then(|w| w.rsplit('/').next())
                .map(|name| root.join("worktrees").join(name))
                .map(|p| p.to_string_lossy().into_owned());
            let stale_worktree = match (&wanted_worktree, object.get("worktree").and_then(Value::as_str)) {
                (Some(wanted), Some(current)) => wanted != current,
                _ => false,
            };
            let stale_base = object.get("base_commit").and_then(Value::as_str) != Some(base);
            let has_kernel = object
                .get("kernel_commit")
                .and_then(Value::as_str)
                .is_some_and(|k| !k.is_empty());
            if !stale_base && !has_kernel && !stale_worktree {
                continue;
            }
            if let Some(wanted) = wanted_worktree {
                object.insert("worktree".into(), Value::String(wanted));
            }
            object.insert("base_commit".into(), Value::String(base.to_string()));
            object.insert("kernel_commit".into(), Value::String(String::new()));
            pending.push((session, serde_json::to_vec(&parsed)?));
        }
    }

    if pending.is_empty() || !apply {
        return Ok(pending.len());
    }
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(BRANCHES)?;
        for (session, bytes) in &pending {
            table.insert(session.as_str(), bytes.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(pending.len())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let apply = args.iter().any(|a| a == "--apply");
    let mut paths: Vec<&String> = Vec::new();
    let mut skip_next = false;
    for a in &args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--repoint-branches" {
            skip_next = true;
            continue;
        }
        if !a.starts_with("--") {
            paths.push(a);
        }
    }
    if paths.len() != 2 {
        eprintln!(
            "usage: migrate-legacy-names <data-dir> <artifacts-dir> [--apply] \
             [--repoint-branches <trunk-commit>]"
        );
        std::process::exit(2);
    }

    let data = Path::new(paths[0]);
    let artifacts = Path::new(paths[1]);
    let db_path = if data.is_file() {
        data.to_path_buf()
    } else {
        data.join("thetis.redb")
    };

    let repoint = args
        .iter()
        .position(|a| a == "--repoint-branches")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let revisions = migrate_revisions(&db_path, apply)?;
    let cache = migrate_build_cache(&artifacts.join("cache"), apply)?;

    if let Some(base) = &repoint {
        // Canonicalised, because the checkout path written into each row is
        // used verbatim as a worker's root. Deriving it from a relative
        // argument silently stores a relative path, and the worker then
        // resolves it against its own working directory — which is the
        // worktree itself, so every lookup lands somewhere that does not
        // exist and the conversation cannot start.
        let root = std::fs::canonicalize(data)
            .with_context(|| format!("resolving {}", data.display()))?
            .parent()
            .map(Path::to_path_buf)
            .context("the data directory has no parent")?;
        let n = repoint_branches(&db_path, base, &root, apply)?;
        let verb = if apply { "re-pointed" } else { "would re-point" };
        println!("{verb} {n} conversation branches at {base}");
    }

    let verb = if apply { "migrated" } else { "would migrate" };
    println!("{verb} {revisions} revision rows in {}", db_path.display());
    println!("{verb} {cache} build-cache entries under {}", artifacts.display());
    if !apply {
        println!("\nnothing was written — re-run with --apply");
        return Ok(());
    }

    let (rows, entries) = verify(&db_path, &artifacts.join("cache"))?;
    println!("verified {rows} revision rows and {entries} build-cache entries decode");
    Ok(())
}
