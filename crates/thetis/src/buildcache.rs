//! Content-addressed build artifacts.
//!
//! Git carries the source history, but two things live outside it: the built
//! component (so a worker can boot a revision without a toolchain) and the
//! smoke-test verdict (so "last known good" survives restarts). Both are
//! stored here, keyed by the content that produced them — the git tree oid of
//! the aspect's source directory combined with everything else that feeds the
//! build (the WIT contract, the lockfile). The same source in two branches
//! therefore shares one artifact, and a checkout whose key is cached loads
//! instantly.
//!
//! Entries are write-once: a key names exactly one build output, so nothing
//! is ever overwritten, and concurrent writers of the same key are benign.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The verdict of the post-build smoke test, frozen with the artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmokeVerdict {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildMeta {
    /// Aspect key ("agent", "gateway/web", "tool/x") or "kernel".
    pub aspect: String,
    /// The cache key the entry lives under.
    pub key: String,
    /// sha256 of the stored artifact, for integrity checks on load.
    pub artifact_sha256: String,
    pub smoke: SmokeVerdict,
    /// The commit whose checkout produced this build. Informational: the key
    /// is the tree, so many commits may map here.
    pub source_commit: String,
    /// The tree identities behind the key, queryable without knowing which
    /// kernel's fingerprint sealed it — how the merge gate recognises a green
    /// build made under a different contract.
    #[serde(default)]
    pub aspect_tree: String,
    #[serde(default)]
    pub wit_tree: String,
    pub created_ms: u64,
    /// Human-readable origin, mirroring what revision notes carried.
    #[serde(default)]
    pub note: String,
}

/// The on-disk store: `<root>/<aspect-key>/<cache-key>/{<artifact>, meta.json}`.
#[derive(Debug, Clone)]
pub struct BuildCache {
    root: PathBuf,
}

impl BuildCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Combine the oids that feed a build into one key. Order matters and is
    /// fixed; any participant changing changes the key.
    pub fn cache_key(parts: &[&str]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hex::encode(&hasher.finalize()[..16])
    }

    fn entry_dir(&self, aspect_key: &str, key: &str) -> PathBuf {
        self.root.join(aspect_key).join(key)
    }

    /// Store `artifact` under `(aspect, key)`. Write-once: when the entry
    /// already exists the stored copy wins and the existing meta is returned,
    /// so racing builders of identical source converge instead of colliding.
    pub fn store(
        &self,
        artifact: &Path,
        artifact_name: &str,
        meta: &BuildMeta,
    ) -> Result<BuildMeta> {
        let dir = self.entry_dir(&meta.aspect, &meta.key);
        if let Some(existing) = self.lookup(&meta.aspect, &meta.key)? {
            return Ok(existing);
        }
        let parent = dir
            .parent()
            .context("cache entry has no parent directory")?;
        fs::create_dir_all(parent)?;

        // Stage next to the destination so the rename is atomic.
        let stage = parent.join(format!(".tmp-{}-{}", meta.key, std::process::id()));
        if stage.exists() {
            fs::remove_dir_all(&stage)?;
        }
        fs::create_dir_all(&stage)?;
        fs::copy(artifact, stage.join(artifact_name))
            .with_context(|| format!("copying {} into the build cache", artifact.display()))?;
        fs::write(stage.join("meta.json"), serde_json::to_vec_pretty(meta)?)?;

        match fs::rename(&stage, &dir) {
            Ok(()) => Ok(meta.clone()),
            Err(_) if dir.exists() => {
                // Lost a benign race; the other writer stored the same content.
                let _ = fs::remove_dir_all(&stage);
                self.lookup(&meta.aspect, &meta.key)?
                    .context("cache entry vanished after a store race")
            }
            Err(err) => {
                let _ = fs::remove_dir_all(&stage);
                Err(err).with_context(|| format!("publishing cache entry {}", dir.display()))
            }
        }
    }

    pub fn lookup(&self, aspect_key: &str, key: &str) -> Result<Option<BuildMeta>> {
        let meta_path = self.entry_dir(aspect_key, key).join("meta.json");
        if !meta_path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&meta_path)?;
        let meta: BuildMeta = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", meta_path.display()))?;
        Ok(Some(meta))
    }

    /// Absolute path of a cached artifact, verified to exist and match its
    /// recorded hash — a half-written or corrupted entry must never load.
    pub fn artifact_path(
        &self,
        meta: &BuildMeta,
        artifact_name: &str,
    ) -> Result<PathBuf> {
        let path = self.entry_dir(&meta.aspect, &meta.key).join(artifact_name);
        if !path.is_file() {
            bail!("cache entry {} has no artifact {artifact_name}", meta.key);
        }
        let actual = hash_file(&path)?;
        if actual != meta.artifact_sha256 {
            bail!(
                "cache entry {} failed its integrity check ({} != {})",
                path.display(),
                actual,
                meta.artifact_sha256
            );
        }
        Ok(path)
    }

    /// Every entry recorded for an aspect, newest first — what "last known
    /// good" and the /admin tables walk.
    pub fn list(&self, aspect_key: &str) -> Result<Vec<BuildMeta>> {
        let dir = self.root.join(aspect_key);
        let mut entries = Vec::new();
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => return Ok(entries),
        };
        for entry in read.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".tmp-") {
                continue;
            }
            if let Some(meta) = self.lookup(aspect_key, &name)? {
                entries.push(meta);
            }
        }
        entries.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
        Ok(entries)
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn meta(aspect: &str, key: &str, sha: &str, smoke: SmokeVerdict) -> BuildMeta {
        BuildMeta {
            aspect: aspect.to_string(),
            key: key.to_string(),
            artifact_sha256: sha.to_string(),
            smoke,
            source_commit: "abc1234".to_string(),
            aspect_tree: String::new(),
            wit_tree: String::new(),
            created_ms: now_ms(),
            note: "test".to_string(),
        }
    }

    #[test]
    fn store_lookup_roundtrip_with_integrity() {
        let tmp = TempDir::new().unwrap();
        let cache = BuildCache::new(tmp.path().join("cache"));
        let wasm = tmp.path().join("component.wasm");
        fs::write(&wasm, b"pretend wasm").unwrap();
        let sha = hash_file(&wasm).unwrap();

        let m = meta("tool/demo", "k1", &sha, SmokeVerdict::Pass);
        cache.store(&wasm, "component.wasm", &m).unwrap();

        let found = cache.lookup("tool/demo", "k1").unwrap().unwrap();
        assert_eq!(found.artifact_sha256, sha);
        let path = cache.artifact_path(&found, "component.wasm").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"pretend wasm");
    }

    #[test]
    fn entries_are_write_once() {
        let tmp = TempDir::new().unwrap();
        let cache = BuildCache::new(tmp.path().join("cache"));
        let wasm = tmp.path().join("a.wasm");
        fs::write(&wasm, b"first").unwrap();
        let sha = hash_file(&wasm).unwrap();
        cache
            .store(&wasm, "component.wasm", &meta("agent", "k", &sha, SmokeVerdict::Pass))
            .unwrap();

        // A second store of the same key keeps the original.
        fs::write(&wasm, b"second").unwrap();
        let winner = cache
            .store(
                &wasm,
                "component.wasm",
                &meta("agent", "k", "other", SmokeVerdict::Fail),
            )
            .unwrap();
        assert_eq!(winner.artifact_sha256, sha);
        assert_eq!(winner.smoke, SmokeVerdict::Pass);
    }

    #[test]
    fn corrupted_artifacts_refuse_to_load() {
        let tmp = TempDir::new().unwrap();
        let cache = BuildCache::new(tmp.path().join("cache"));
        let wasm = tmp.path().join("c.wasm");
        fs::write(&wasm, b"good bytes").unwrap();
        let sha = hash_file(&wasm).unwrap();
        let m = meta("gateway/web", "k9", &sha, SmokeVerdict::Pass);
        cache.store(&wasm, "component.wasm", &m).unwrap();

        // Flip the stored artifact behind the cache's back.
        let stored = tmp
            .path()
            .join("cache")
            .join("gateway/web")
            .join("k9")
            .join("component.wasm");
        fs::write(&stored, b"evil bytes").unwrap();
        assert!(cache.artifact_path(&m, "component.wasm").is_err());
    }

    #[test]
    fn list_is_newest_first_and_skips_staging_dirs() {
        let tmp = TempDir::new().unwrap();
        let cache = BuildCache::new(tmp.path().join("cache"));
        let wasm = tmp.path().join("d.wasm");
        fs::write(&wasm, b"x").unwrap();
        let sha = hash_file(&wasm).unwrap();

        let mut old = meta("agent", "old", &sha, SmokeVerdict::Fail);
        old.created_ms = 1000;
        let mut new = meta("agent", "new", &sha, SmokeVerdict::Pass);
        new.created_ms = 2000;
        cache.store(&wasm, "component.wasm", &old).unwrap();
        cache.store(&wasm, "component.wasm", &new).unwrap();
        fs::create_dir_all(tmp.path().join("cache/agent/.tmp-zzz-1")).unwrap();

        let listed = cache.list("agent").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].key, "new");
        assert_eq!(listed[1].key, "old");
        assert!(cache.list("tool/none").unwrap().is_empty());
    }

    #[test]
    fn cache_keys_are_order_and_content_sensitive() {
        let a = BuildCache::cache_key(&["tree1", "wit1", "lock1"]);
        assert_eq!(a, BuildCache::cache_key(&["tree1", "wit1", "lock1"]));
        assert_ne!(a, BuildCache::cache_key(&["tree2", "wit1", "lock1"]));
        assert_ne!(a, BuildCache::cache_key(&["wit1", "tree1", "lock1"]));
        // Concatenation ambiguity must not collide ("ab","c" vs "a","bc").
        assert_ne!(
            BuildCache::cache_key(&["ab", "c"]),
            BuildCache::cache_key(&["a", "bc"])
        );
    }
}
