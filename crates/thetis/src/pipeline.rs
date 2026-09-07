//! The one path a change takes to reach the running system.
//!
//! build -> commit -> cache -> validate -> swap
//!
//! Human edits (via the file watcher) and the agent's own self-modification
//! both go through here, so both get identical guarantees: a candidate that
//! fails any gate never becomes active, and whatever was already running
//! keeps running.
//!
//! Versioning is the conversation's git branch: every green build is a
//! commit, and the built artifact plus its smoke-test verdict are stored in
//! a content-addressed cache keyed by the tree that produced them. The same
//! source therefore builds once across every branch that holds it, a
//! checkout whose key is cached loads with no toolchain at all, and "roll
//! back" means putting the tree back at a commit whose key is green.
//!
//! Swapping is safe at any moment because guests are instantiated per call. A
//! turn already in flight holds its own `Arc` to the old component and finishes
//! on it; the next call picks up the new one.

use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use std::time::Instant;
use wasmtime::component::Component;

use crate::aspect::Aspect;
use crate::buildcache::{BuildCache, BuildMeta, SmokeVerdict};
use crate::builder::BuildOptions;
use crate::config::Config;
use crate::grip::Grip;
use crate::loader::Loader;
use crate::revisions::Origin;
use crate::runtime::{Budget, Caps};

/// The artifact filename inside a cache entry.
pub const CACHE_ARTIFACT: &str = "component.wasm";

/// The contract this kernel was compiled against, as a fingerprint.
///
/// A guest artifact's smoke verdict is only valid under the bindings that
/// issued it: a component built and validated green by a branch kernel with a
/// different WIT can be poison to this one, and content-addressing by source
/// alone once let exactly that load without a gate. The kernel's own compiled-
/// in copy of the contract is the identity that matters, so it keys the cache.
pub fn kernel_wit_fingerprint() -> &'static str {
    static FP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FP.get_or_init(|| wit_fingerprint(include_str!("../../../wit/thetis.wit")))
}

/// The contract a checkout's guests will bind against.
///
/// Every guest crate generates its bindings from the `wit/` directory beside
/// it (`path: "../../wit"`), so for a worker this is the *worktree's* copy,
/// not trunk's and not the kernel's. That distinction is the whole point: a
/// branch that has not merged a trunk WIT change still holds the old contract,
/// and every component built there imports it.
pub fn checkout_wit_fingerprint(cfg: &Config) -> Option<String> {
    std::fs::read_to_string(cfg.paths.wit.join("thetis.wit"))
        .ok()
        .map(|text| wit_fingerprint(&text))
}

/// Shared so the two fingerprints are always comparable: hashing the kernel's
/// copy one way and a checkout's another would make every comparison a lie.
fn wit_fingerprint(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(&digest[..8])
}

/// Where a checkout's WIT contract stands against the kernel that has to load
/// the guests built from it.
#[derive(Debug, Clone)]
pub enum ContractCheck {
    /// The checkout binds against the contract this kernel speaks.
    Match,
    /// The checkout was behind and has been brought up to trunk.
    Repaired {
        from: String,
        to: String,
        commits: u64,
    },
    /// The checkout disagrees and trunk cannot settle it, because trunk does
    /// not match the kernel either. The binary is the stale one, so nothing
    /// was moved — moving would not have helped.
    KernelStale {
        kernel: String,
        checkout: String,
        trunk: String,
    },
    /// The checkout disagrees, trunk holds the fix, but it would not merge.
    /// The branch is left exactly as it was found.
    Unrepaired {
        kernel: String,
        checkout: String,
        detail: String,
    },
    /// The checkout disagrees and this role has no git to settle it with. The
    /// gateway is the case that matters: it holds no `GitCtl` at all, and the
    /// checkout it reads is trunk itself — so a mismatch here is never a stale
    /// branch, it is a stale binary, and rebuilding is the only way out.
    Unrepairable { kernel: String, checkout: String },
    /// Nothing to check: no `wit/thetis.wit` to read.
    Unknown,
}

/// Brings a checkout's WIT contract into line with the kernel, before anything
/// is built against it.
///
/// The kernel's contract is compiled in; a guest's comes from the `wit/`
/// directory of whatever checkout it is built in. Those are two different
/// places, and a trunk WIT change moves only one of them — so every branch
/// that has not merged it goes on building guests against the old variant, and
/// wasmtime refuses them at instantiation ("expected variant of 15 cases,
/// found 14").
///
/// Nothing downstream can recover from that, which is why the check belongs
/// here rather than in the error paths. The build itself succeeds, so the
/// compiler raises nothing. The smoke test does reject the artifact, correctly.
/// But the fallback then searches *branch* history for a green build, and that
/// is the one axis guaranteed to be equally stale — every artifact it can find
/// binds the same old contract. The branch has no way back on its own, and the
/// worker comes up with no agent at all.
///
/// Trunk is the only place the new contract exists, so merging trunk is the
/// repair — applied once, before the first wasted compile.
pub async fn reconcile_wit_contract(grip: &Arc<Grip>) -> ContractCheck {
    reconcile_against(
        kernel_wit_fingerprint(),
        checkout_wit_fingerprint(&grip.cfg()).as_deref(),
        grip.git.as_ref(),
    )
    .await
}

/// The decision itself, over the three things it actually depends on: what the
/// kernel speaks, what the checkout holds, and whether this role has git to
/// settle a disagreement with. Taking them as arguments rather than reaching
/// into a `Grip` is what lets the gateway's case — `git` absent — be tested at
/// all; it is the one that previously fell through to silence.
async fn reconcile_against(
    kernel: &str,
    checkout: Option<&str>,
    git: Option<&crate::gitctl::GitCtl>,
) -> ContractCheck {
    let kernel = kernel.to_string();
    let Some(checkout) = checkout.map(str::to_string) else {
        return ContractCheck::Unknown;
    };
    if checkout == kernel {
        return ContractCheck::Match;
    }
    let Some(git) = git else {
        return ContractCheck::Unrepairable { kernel, checkout };
    };

    // Does trunk even hold what the kernel speaks? If it does not, the
    // checkout is not the thing that is behind — the binary is — and a merge
    // would shuffle a branch around to no effect while hiding the real cause.
    let trunk = crate::branchops::trunk_ref();
    let trunk_fp = match git
        .run_raw(&["show", &format!("{trunk}:wit/thetis.wit")])
        .await
    {
        Ok(out) if out.status.success() => wit_fingerprint(&String::from_utf8_lossy(&out.stdout)),
        _ => String::new(),
    };
    if trunk_fp != kernel {
        return ContractCheck::KernelStale {
            kernel,
            checkout,
            trunk: trunk_fp,
        };
    }

    match update_to_trunk(git, &trunk).await {
        Ok(commits) => ContractCheck::Repaired {
            from: checkout,
            to: kernel,
            commits,
        },
        Err(e) => ContractCheck::Unrepaired {
            kernel,
            checkout,
            detail: format!("{e:#}"),
        },
    }
}

/// Moves this checkout onto trunk, by the most lossless route available.
///
/// A branch that has committed nothing of its own — the common case for one
/// that has merely gone stale — fast-forwards, which cannot conflict and
/// cannot lose work. A branch carrying its own commits needs a real merge, and
/// a conflicting one is abandoned rather than left half-resolved: a worker
/// coming up is the worst possible moment to hand somebody a conflicted tree,
/// and the branch is far more useful untouched and reported than wedged.
async fn update_to_trunk(git: &crate::gitctl::GitCtl, trunk: &str) -> Result<u64> {
    if git.merge_in_progress().await? {
        anyhow::bail!("a merge is already in progress in this checkout");
    }
    let (_, behind) = git.ahead_behind("HEAD", trunk).await?;

    // Uncommitted work would block the merge outright. Checkpointing it is
    // what an interactive update from trunk does for the same reason, and it
    // leaves the edits recoverable instead of in the way.
    if git.is_dirty().await.unwrap_or(false) {
        git.add_all_and_commit("checkpoint: before the WIT contract update")
            .await?;
    }

    if git.merge_ff_only(trunk).await.is_ok() {
        return Ok(behind);
    }
    match git.merge(trunk, "update from trunk (WIT contract)").await? {
        crate::gitctl::MergeOutcome::Clean { .. } => Ok(behind),
        crate::gitctl::MergeOutcome::Conflict { paths } => {
            // Put the tree back before reporting; a conflicted checkout would
            // fail every later git operation this worker attempts.
            git.merge_abort().await.ok();
            anyhow::bail!(
                "merging trunk conflicts in {} file(s): {}",
                paths.len(),
                paths.join(", ")
            )
        }
    }
}

impl ContractCheck {
    /// Says what was found, at the severity it deserves.
    ///
    /// Logged once at startup and deliberately explicit: a mismatch here is
    /// the cause of every smoke-test failure that follows, and the old
    /// behaviour made a reader infer that from a wall of variant-arity errors.
    pub fn report(&self) {
        match self {
            ContractCheck::Match => tracing::debug!(
                contract = kernel_wit_fingerprint(),
                "WIT contract matches the kernel"
            ),
            ContractCheck::Repaired { from, to, commits } => tracing::warn!(
                %from, %to, commits,
                "this checkout held an older WIT contract than the kernel; \
                 updated from trunk before building anything against it"
            ),
            ContractCheck::KernelStale {
                kernel,
                checkout,
                trunk,
            } => tracing::error!(
                %kernel, %checkout, %trunk,
                "the kernel binary is older than the WIT in this checkout; guests built here \
                 cannot load until it is rebuilt and restarted (scripts/rebuild-kernel.sh)"
            ),
            ContractCheck::Unrepaired {
                kernel,
                checkout,
                detail,
            } => tracing::error!(
                %kernel, %checkout, %detail,
                "this checkout's WIT contract does not match the kernel and trunk would not merge in; \
                 guests built here will fail their smoke test until this is resolved"
            ),
            ContractCheck::Unrepairable { kernel, checkout } => tracing::error!(
                %kernel, %checkout,
                "the running kernel binary was built against a different WIT contract than this \
                 checkout holds; every guest built from it will be refused at instantiation until \
                 the kernel is rebuilt and restarted (scripts/rebuild-kernel.sh)"
            ),
            ContractCheck::Unknown => {}
        }
    }

    /// Whether guests built in this checkout can be expected to load at all.
    /// A caller that is about to explain a failure uses this to say *why*.
    pub fn is_sound(&self) -> bool {
        matches!(
            self,
            ContractCheck::Match | ContractCheck::Repaired { .. } | ContractCheck::Unknown
        )
    }
}

/// The cache key for an aspect's build as of `rev`: the tree that is the
/// source, the WIT contract it binds against, and the build settings.
/// `None` when the checkout is not a git repo or the aspect is untracked there.
pub async fn aspect_cache_key(grip: &Arc<Grip>, rev: &str, aspect: &Aspect) -> Option<String> {
    cache_key_with(grip.git.as_ref()?, &grip.cfg(), rev, aspect).await
}

/// As [`aspect_cache_key`], for callers holding a checkout that is not this
/// process's own — the gateway keying trunk, the merge engine keying a branch.
pub async fn cache_key_with(
    git: &crate::gitctl::GitCtl,
    cfg: &crate::config::Config,
    rev: &str,
    aspect: &Aspect,
) -> Option<String> {
    let rel = cfg.aspect_source_rel(aspect)?;
    let aspect_tree = git.tree_oid(rev, &rel).await.ok()??;
    let wit_tree = git.tree_oid(rev, "wit").await.ok()?.unwrap_or_default();
    Some(BuildCache::cache_key(&[
        &aspect_tree,
        &wit_tree,
        kernel_wit_fingerprint(),
        &cfg.build.target,
        &cfg.build.profile,
    ]))
}

/// A loader revision number derived from a cache key, so "did the component
/// change" comparisons keep working without a global counter.
pub fn key_revision(key: &str) -> u64 {
    u64::from_str_radix(key.get(..16).unwrap_or("0"), 16).unwrap_or(0)
}

/// The agent, plus every gateway and tool with a crate in the configured
/// directories — the set of aspects a checkout defines.
pub fn discover_aspects(cfg: &crate::config::Config) -> Vec<Aspect> {
    let mut aspects = vec![Aspect::Agent];

    // Concrete wrappers: the generic constructors cannot coerce to a fn pointer.
    fn gateway(name: &str) -> Aspect {
        Aspect::Gateway(name.to_string())
    }
    fn tool(name: &str) -> Aspect {
        Aspect::Tool(name.to_string())
    }

    let sources: [(&std::path::Path, &str, fn(&str) -> Aspect); 2] = [
        (&cfg.paths.gateways, &cfg.paths.gateway_prefix, gateway),
        (&cfg.paths.tools, &cfg.paths.tool_prefix, tool),
    ];

    for (dir, prefix, make) in sources {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().join("Cargo.toml").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // The prefix is a naming convention, not part of the aspect's name.
            if let Some(short) = name.strip_prefix(prefix) {
                aspects.push(make(short));
            }
        }
    }

    // The gateway comes up first so the UI is reachable as early as possible.
    aspects.sort_by_key(|s| match s {
        Aspect::Gateway(_) => 0,
        Aspect::Agent => 1,
        Aspect::Tool(_) => 2,
    });
    aspects
}

/// A fingerprint of everything a build's success depends on, for the negative
/// cache.
///
/// Deliberately independent of git: the build cache's key needs a committed
/// tree, but a *failing* build never commits, and the failures worth
/// suppressing are exactly those. So this hashes what is on disk.
///
/// Covers the aspect's own sources, the WIT contract, and this kernel's
/// contract fingerprint. The contract matters because the loop this was
/// written for was a smoke test failing on a contract mismatch: the aspect's
/// own files were not changing, so a source-only fingerprint would have
/// suppressed the retry correctly, but a later `wit/` fix must un-suppress it.
fn source_fingerprint(cfg: &Config, aspect: &Aspect) -> Option<String> {
    fingerprint_dirs(&[cfg.aspect_source_dir(aspect), cfg.paths.wit.clone()])
}

/// The hashing behind [`source_fingerprint`], over an explicit set of roots so
/// it can be tested without assembling a whole `Config`.
fn fingerprint_dirs(roots: &[std::path::PathBuf]) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for root in roots {
        collect_files(root, &mut files);
    }
    if files.is_empty() {
        return None;
    }
    // Directory order is not stable across filesystems; the hash must be.
    files.sort();

    let mut hasher = Sha256::new();
    hasher.update(kernel_wit_fingerprint().as_bytes());
    hasher.update([0]);
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            // Unreadable mid-build: treat the tree as un-fingerprintable
            // rather than hashing a partial view and suppressing wrongly.
            return None;
        };
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&bytes);
        hasher.update([0]);
    }
    Some(hex::encode(&hasher.finalize()[..16]))
}

/// Every file under a directory, recursively, skipping build output.
fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "target" || name.starts_with("target-") || name == ".git" {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The result of pushing a change through the pipeline, shaped so it can be
/// handed straight back to the model as a compile report.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub success: bool,
    pub aspect: String,
    pub revision: Option<u64>,
    pub stderr: String,
    pub duration_ms: u64,
    pub detail: String,
    pub pending_swap: bool,
}

impl Outcome {
    fn failure(
        aspect: &Aspect,
        detail: impl Into<String>,
        stderr: String,
        started: Instant,
    ) -> Self {
        Self {
            success: false,
            aspect: aspect.key(),
            revision: None,
            stderr,
            duration_ms: started.elapsed().as_millis() as u64,
            detail: detail.into(),
            pending_swap: false,
        }
    }
}

/// Files a smoke-passing artifact in the build cache under the tree that
/// produced it, so every branch holding that tree — and the merge gate — can
/// recognise it as green without a toolchain.
///
/// Called from both green exits of the pipeline: the one where the component
/// changed, and the one where cargo produced bytes identical to what is already
/// serving. The second is easy to overlook and was the source of a merge
/// deadlock, so the two paths share this function rather than each remembering
/// to cache.
///
/// Keyed off `HEAD`, so it must be called *after* the source has been
/// committed: the key names the committed tree, and caching against a tree that
/// does not exist yet would file the artifact where nothing will look for it.
async fn cache_green(
    grip: &Arc<Grip>,
    aspect: &Aspect,
    wasm: &std::path::Path,
    artifact_sha256: &str,
    source_commit: Option<String>,
    note: &str,
    origin: Origin,
) {
    let Some(key) = aspect_cache_key(grip, "HEAD", aspect).await else {
        return;
    };
    let (aspect_tree, wit_tree) = match (&grip.git, grip.cfg().aspect_source_rel(aspect)) {
        (Some(git), Some(rel)) => (
            git.tree_oid("HEAD", &rel)
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
            git.tree_oid("HEAD", "wit")
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
        ),
        _ => (String::new(), String::new()),
    };
    let meta = BuildMeta {
        aspect: aspect.key(),
        key,
        artifact_sha256: artifact_sha256.to_string(),
        smoke: SmokeVerdict::Pass,
        source_commit: source_commit.unwrap_or_default(),
        aspect_tree,
        wit_tree,
        created_ms: crate::buildcache::now_ms(),
        note: format!("{}: {note}", origin.label()),
    };
    if let Err(e) = grip.buildcache.store(wasm, CACHE_ARTIFACT, &meta) {
        // A cache miss later costs a rebuild, not correctness.
        tracing::warn!(%aspect, error = %e, "could not cache the artifact");
    }
}

/// Builds an aspect's current source and, if every gate passes, puts it live.
pub async fn build_and_activate(
    grip: &Arc<Grip>,
    aspect: &Aspect,
    origin: Origin,
    note: &str,
) -> Result<Outcome> {
    build_and_activate_with(grip, aspect, origin, note, BuildOptions::default()).await
}

/// As [`build_and_activate`], with control over how cargo is invoked.
pub async fn build_and_activate_with(
    grip: &Arc<Grip>,
    aspect: &Aspect,
    origin: Origin,
    note: &str,
    opts: BuildOptions,
) -> Result<Outcome> {
    let started = Instant::now();

    // A build already running for this aspect will pick up the same source tree,
    // so waiting for the lock only to repeat it is wasted work.
    let Some(_in_flight) = grip.begin_build(aspect) else {
        return Ok(Outcome::failure(
            aspect,
            "a build for this aspect is already running; its result will cover this change too",
            String::new(),
            started,
        ));
    };

    // 0. Has the source been deleted? A tool whose crate is gone must leave
    //    service, or the loader serves the last artifact forever: the tool stays
    //    callable, and every future rebuild fails with "no crate found".
    //    Restricted to tools on purpose — the agent and the gateway have no
    //    unloaded state worth having, so for those a missing crate is a fault to
    //    report, not an instruction to remove ourselves.
    if matches!(aspect, Aspect::Tool(_))
        && !grip
            .cfg()
            .aspect_source_dir(aspect)
            .join("Cargo.toml")
            .is_file()
    {
        if grip.loader.get(aspect).is_some() {
            grip.uninstall_component(aspect);
            let _ = grip
                .commit_worktree(&format!("{}: removed {aspect}", origin.label()))
                .await;
            return Ok(Outcome {
                success: true,
                aspect: aspect.key(),
                revision: None,
                stderr: String::new(),
                duration_ms: started.elapsed().as_millis() as u64,
                detail: "the crate is gone, so the tool was unloaded and deregistered".to_string(),
                pending_swap: false,
            });
        }
        return Ok(Outcome::failure(
            aspect,
            "no crate at this path, and nothing loaded to remove",
            String::new(),
            started,
        ));
    }

    // 1. Has this exact tree already been built, by any branch? The cache is
    //    keyed by content — the aspect's tree, the WIT contract, this kernel's
    //    fingerprint and the build settings — so a hit is the same artifact
    //    cargo would produce, already smoke-tested. Skipping cargo here is what
    //    makes "update from trunk" and "reset to a green commit" instant rather
    //    than a full compile each time.
    //
    //    Only for a clean tree: with uncommitted edits, HEAD's key names a tree
    //    that is not what is on disk, and loading the cached artifact would
    //    silently serve something the source no longer says.
    let clean = match &grip.git {
        Some(git) => !git.is_dirty().await.unwrap_or(true),
        None => false,
    };
    if clean {
        if let Some(key) = aspect_cache_key(grip, "HEAD", aspect).await {
            if let Ok(Some(meta)) = grip.buildcache.lookup(&aspect.key(), &key) {
                if meta.smoke == SmokeVerdict::Pass {
                    let revision = key_revision(&key);
                    let already = grip
                        .loader
                        .get(aspect)
                        .is_some_and(|loaded| loaded.revision == revision);
                    if already {
                        return Ok(Outcome {
                            success: true,
                            aspect: aspect.key(),
                            revision: Some(revision),
                            stderr: String::new(),
                            duration_ms: started.elapsed().as_millis() as u64,
                            detail: "no change: this tree is already serving".to_string(),
                            pending_swap: false,
                        });
                    }
                    match grip
                        .buildcache
                        .artifact_path(&meta, CACHE_ARTIFACT)
                        .and_then(|artifact| {
                            Loader::compile(&grip.runtime.engine, aspect, revision, &artifact)
                        }) {
                        Ok(compiled) => {
                            grip.install_component(compiled).await;
                            tracing::info!(%aspect, revision, "served from the build cache; no compile needed");
                            return Ok(Outcome {
                                success: true,
                                aspect: aspect.key(),
                                revision: Some(revision),
                                stderr: String::new(),
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: "loaded from the build cache; this tree was already built"
                                    .to_string(),
                                pending_swap: matches!(aspect, Aspect::Agent),
                            });
                        }
                        Err(e) => {
                            // A corrupt or contract-mismatched entry is not
                            // fatal; fall through and compile properly.
                            tracing::warn!(%aspect, error = %e,
                                "cached artifact would not load; compiling instead");
                        }
                    }
                }
            }
        }
    }

    // 2. Has this exact source already failed? A failed build never commits and
    //    never caches, so without this the same broken tree is recompiled on
    //    every file event, forever, at full CPU. Refusing costs nothing: an
    //    identical input cannot produce a different result.
    //
    //    `refresh_lockfile` means the caller is deliberately retrying after
    //    changing dependencies, so it always gets a real attempt.
    let fingerprint = source_fingerprint(&grip.cfg(), aspect);
    if !opts.refresh_lockfile {
        if let Some(fp) = &fingerprint {
            if let Some(previous) = grip.known_bad_build(aspect, fp) {
                tracing::debug!(%aspect, "skipping rebuild: this exact source already failed");
                return Ok(Outcome::failure(
                    aspect,
                    format!(
                        "this exact source already failed to build, so it was not \
                         compiled again: {previous}. Change the source, or fix the \
                         contract, to trigger a real attempt."
                    ),
                    String::new(),
                    started,
                ));
            }
        }
    }

    // A new fingerprint deserves a clean slate, so a tree that fails in a
    // different way reports its newest error rather than the first.
    grip.clear_build_failure(aspect);

    // Remembers a failure against the source that caused it. Every gate below
    // funnels through here so no failure path can forget to record itself.
    let remember = |detail: String, stderr: String| {
        if let Some(fp) = &fingerprint {
            grip.record_build_failure(aspect, fp, &detail);
        }
        Ok(Outcome::failure(aspect, detail, stderr, started))
    };

    // 3. Compile.
    let build = grip.builder.build_with(&grip.cfg(), aspect, opts).await?;
    if !build.success {
        return remember(
            "compilation failed; the running revision is unchanged".to_string(),
            build.stderr,
        );
    }
    let wasm = build
        .wasm_path
        .context("build reported success without an artifact")?;

    // 4. If cargo produced a byte-identical component and it is what the
    //    loader is actually serving, nothing changed — but the *source* may
    //    still have (a comment, whitespace), so checkpoint it either way.
    let fresh_hash = crate::buildcache::hash_file(&wasm).unwrap_or_default();
    let already_serving = grip
        .loader
        .get(aspect)
        .is_some_and(|loaded| loaded.artifact_sha256 == fresh_hash);
    if already_serving {
        let commit = grip
            .commit_worktree(&format!("{}: {note} ({aspect})", origin.label()))
            .await
            .ok()
            .flatten();
        // File the artifact under the tree that just got committed, exactly as
        // a changed build would.
        //
        // Skipping this was a real bug, not an optimisation. The merge gate
        // recognises a green build by *tree identity*, so a commit with no
        // cache entry is indistinguishable from one that was never built — and
        // this path is reached precisely by the edits that do not alter the
        // compiled output: a reworded comment, whitespace, or a rebuild after
        // `update_from_trunk` brought in changes to other aspects only. Every
        // such commit moved the tree, cached nothing, and left the branch
        // permanently unmergeable with "its latest state has no green build".
        // Rebuilding could never clear it: the second attempt is byte-identical
        // too, so it landed here again.
        //
        // Claiming `Pass` without re-running the smoke test is sound here and
        // only here: these bytes are byte-identical to what the loader is
        // serving, and a component only becomes live by passing the smoke test
        // under a cache key carrying *this* kernel's contract fingerprint. Same
        // bytes, same kernel, same verdict.
        cache_green(grip, aspect, &wasm, &fresh_hash, commit, note, origin).await;
        return Ok(Outcome {
            success: true,
            aspect: aspect.key(),
            revision: None,
            stderr: build.stderr,
            duration_ms: started.elapsed().as_millis() as u64,
            detail: "no change: the build is identical to what is serving".to_string(),
            pending_swap: false,
        });
    }

    // 5. Does wasmtime accept it as a component for this world? Validation
    //    runs before anything is committed: the branch log's last commit must
    //    always be a build that passed every gate.
    let compiled = match Loader::compile(&grip.runtime.engine, aspect, 0, &wasm) {
        Ok(c) => c,
        Err(e) => {
            return remember(
                format!("the build is not a valid component: {e:#}"),
                build.stderr,
            );
        }
    };

    // 6. Does it actually run?
    if let Err(e) = smoke_test(grip, aspect, &compiled.component).await {
        return remember(
            format!("the build compiled but failed its smoke test: {e:#}"),
            build.stderr,
        );
    }

    // 7. Green: freeze the source on the branch, then file the artifact under
    //    the tree that produced it. Every branch holding this tree — and every
    //    later checkout of it — now loads without a toolchain.
    let commit = grip
        .commit_worktree(&format!("{}: {note} ({aspect})", origin.label()))
        .await
        .ok()
        .flatten();
    let key = aspect_cache_key(grip, "HEAD", aspect).await;
    let revision = key.as_deref().map(key_revision).unwrap_or(0);
    cache_green(grip, aspect, &wasm, &fresh_hash, commit, note, origin).await;

    // 8. Live. Installing through the grip keeps the tool registry in step.
    let component = Arc::new(crate::loader::LoadedComponent {
        aspect: compiled.aspect.clone(),
        revision,
        artifact_sha256: compiled.artifact_sha256.clone(),
        component: compiled.component.clone(),
    });
    grip.install_component(component).await;

    Ok(Outcome {
        success: true,
        aspect: aspect.key(),
        revision: Some(revision),
        stderr: build.stderr,
        duration_ms: started.elapsed().as_millis() as u64,
        detail: String::new(),
        // A turn in flight finishes on the old code, so from the agent's point
        // of view its own changes land on the next turn.
        pending_swap: matches!(aspect, Aspect::Agent),
    })
}

/// Exercises a candidate's exports before it is allowed to serve traffic.
///
/// This is what stops a self-modification from making the system unreachable:
/// a component that traps, hangs, or is missing an export never becomes active.
pub(crate) async fn smoke_test(
    grip: &Arc<Grip>,
    aspect: &Aspect,
    component: &Component,
) -> Result<()> {
    let caps = match aspect {
        Aspect::Agent => Caps::Agent,
        Aspect::Gateway(_) => Caps::Gateway,
        Aspect::Tool(_) => Caps::Tool,
    };
    let budget = Budget::probe(format!("{aspect} smoke test"), grip.cfg().probe_budget);
    let mut store = grip.runtime.new_store(grip.clone(), caps, budget, None);
    let linker = grip.runtime.linker(caps);

    match aspect {
        Aspect::Agent => {
            let agent =
                crate::bindings::agent::Agent::instantiate_async(&mut store, component, linker)
                    .await
                    .map_err(anyhow::Error::from)
                    .context("instantiating")?;

            let health = agent
                .call_health(&mut store)
                .await
                .map_err(anyhow::Error::from)
                .context("health probe")?;
            if health.trim().is_empty() {
                anyhow::bail!("health probe returned nothing");
            }
            agent
                .call_describe(&mut store)
                .await
                .map_err(anyhow::Error::from)
                .context("describe")?;
        }

        Aspect::Gateway(_) => {
            let gw =
                crate::bindings::gateway::Gateway::instantiate_async(&mut store, component, linker)
                    .await
                    .map_err(anyhow::Error::from)
                    .context("instantiating")?;

            // A gateway that cannot serve its own index page would leave the
            // user with a blank screen, so that is the gate.
            let index = gw
                .call_serve_asset(&mut store, "/")
                .await
                .map_err(anyhow::Error::from)
                .context("serve-asset")?;
            match index {
                Some(asset) if !asset.bytes.is_empty() => {}
                _ => anyhow::bail!("serve-asset(\"/\") returned no page"),
            }

            gw.call_on_client_message(&mut store, "smoke-test", r#"{"type":"list"}"#)
                .await
                .map_err(anyhow::Error::from)
                .context("on-client-message")?;
        }

        Aspect::Tool(name) => {
            let tool =
                crate::bindings::tool::Tool::instantiate_async(&mut store, component, linker)
                    .await
                    .map_err(anyhow::Error::from)
                    .context("instantiating")?;

            let manifest = tool
                .call_describe(&mut store)
                .await
                .map_err(anyhow::Error::from)
                .context("describe")?;
            if manifest.name.trim().is_empty() {
                anyhow::bail!("tool manifest has no name");
            }
            // A mismatch here would make the tool uncallable: the model would
            // be told one name and the registry keyed by another.
            if &manifest.name != name {
                anyhow::bail!(
                    "tool manifest says '{}' but the aspect is '{name}'",
                    manifest.name
                );
            }
            serde_json::from_str::<serde_json::Value>(&manifest.args_schema_json)
                .context("argument schema is not valid JSON")?;
        }
    }

    Ok(())
}

/// Puts an aspect's source back at this branch's most recent green build and
/// reactivates it — the watchdog's action when a revision keeps failing, and
/// the boot fallback when the tree no longer builds.
///
/// "Green" is defined by the cache: the newest commit whose tree has a
/// stored, smoke-passing artifact. Because the reset is itself a commit, the
/// bad version stays in history — this moves forward to an old tree, it
/// never rewrites anything.
pub async fn reset_aspect_to_green(grip: &Arc<Grip>, aspect: &Aspect) -> Result<String> {
    let git = grip
        .git
        .as_ref()
        .ok_or_else(|| anyhow!("this process has no checkout to reset"))?;
    let rel = grip
        .cfg()
        .aspect_source_rel(aspect)
        .ok_or_else(|| anyhow!("{aspect} lives outside the checkout"))?;

    // The build that keeps failing passed its smoke test once, so it is
    // "green" by the cache's lights — the target is the newest green tree
    // that *differs* from what is serving, i.e. the previous known-good.
    let serving = grip.loader.get(aspect).map(|loaded| loaded.revision);
    let mut target: Option<(String, String)> = None; // (commit, key)
    for commit in git.log("HEAD", 50).await? {
        let Some(key) = aspect_cache_key(grip, &commit.rev, aspect).await else {
            continue;
        };
        if Some(key_revision(&key)) == serving {
            continue;
        }
        if grip.buildcache.lookup(&aspect.key(), &key)?.is_some() {
            target = Some((commit.rev, key));
            break;
        }
    }
    // Nothing green in this branch's history has two very different causes,
    // and saying only the first one sent the last diagnosis of this down the
    // wrong path entirely. A contract mismatch guarantees the search fails —
    // every artifact this branch can offer binds the old WIT — so the reset is
    // not the thing that is broken, and reporting it as such buries the cause.
    let (commit, key) = match target {
        Some(found) => found,
        None => {
            let contract = checkout_wit_fingerprint(&grip.cfg());
            let kernel = kernel_wit_fingerprint();
            if contract.as_deref().is_some_and(|fp| fp != kernel) {
                return Err(anyhow!(
                    "{aspect} has no green build this kernel can load: the checkout's WIT contract \
                     ({}) is not the kernel's ({kernel}), so every artifact in this branch's history \
                     binds the wrong one. Update from trunk rather than resetting.",
                    contract.unwrap_or_default()
                ));
            }
            return Err(anyhow!(
                "{aspect} has no green build in recent branch history"
            ));
        }
    };

    // The watcher would read the restore as a fresh edit and rebuild over it.
    grip.suppress_watch(aspect, grip.cfg().watchdog.watch_suppression);
    git.sync_paths_to(&commit, &rel).await?;
    let short = &commit[..12.min(commit.len())];
    grip.commit_worktree(&format!("watchdog: reset {aspect} to green {short}"))
        .await?;

    // Loading goes through the cache: the artifact for this tree is stored,
    // so no toolchain is needed to put it back in service.
    let meta = grip
        .buildcache
        .lookup(&aspect.key(), &key)?
        .ok_or_else(|| anyhow!("green artifact vanished from the cache"))?;
    let artifact = grip.buildcache.artifact_path(&meta, CACHE_ARTIFACT)?;
    let component = Loader::compile(&grip.runtime.engine, aspect, key_revision(&key), &artifact)?;
    grip.install_component(component).await;

    Ok(format!(
        "{aspect} was reset to its last green build ({short})"
    ))
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The negative cache is only safe if the fingerprint moves whenever the
    /// build's input moves. Too sticky and a fixed tree stays suppressed —
    /// which would be far worse than the spinning it replaced.
    #[test]
    fn the_fingerprint_tracks_source_and_contract_but_not_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("tools/demo");
        let wit = tmp.path().join("wit");
        write(&src, "src/lib.rs", "fn a() {}");
        write(&wit, "thetis.wit", "package thetis:grip;");
        let roots = vec![src.clone(), wit.clone()];

        let first = fingerprint_dirs(&roots).unwrap();

        // Stable across calls: nothing changed, so a retry is recognised.
        assert_eq!(first, fingerprint_dirs(&roots).unwrap());

        // A touch is not a change. This is the whole point — the watcher fires
        // on mtime, and a fingerprint that followed mtime would never suppress.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open(src.join("src/lib.rs"))
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
        assert_eq!(
            first,
            fingerprint_dirs(&roots).unwrap(),
            "a touch must not change the fingerprint"
        );

        // Editing the aspect's source must change it, or a fix stays blocked.
        write(&src, "src/lib.rs", "fn a() { todo!() }");
        let edited = fingerprint_dirs(&roots).unwrap();
        assert_ne!(first, edited, "a source edit must change the fingerprint");

        // Fixing the *contract* must change it too. The real loop was a smoke
        // test failing on a contract mismatch, with the aspect's own files
        // untouched; if `wit/` did not count, repairing it would not release
        // the suppression and the aspect would stay broken until restart.
        write(&wit, "thetis.wit", "package thetis:grip; // fixed");
        assert_ne!(
            edited,
            fingerprint_dirs(&roots).unwrap(),
            "a contract change must change the fingerprint"
        );

        // Adding a file counts.
        let with_extra = fingerprint_dirs(&roots).unwrap();
        write(&src, "src/extra.rs", "fn b() {}");
        assert_ne!(with_extra, fingerprint_dirs(&roots).unwrap());

        // Build output must not count, or every build would invalidate itself
        // and the suppression would never hold.
        let settled = fingerprint_dirs(&roots).unwrap();
        write(&src, "target/debug/junk.o", "binary noise");
        write(&src, "target-wasm/out.wasm", "more noise");
        assert_eq!(
            settled,
            fingerprint_dirs(&roots).unwrap(),
            "build output must not affect the fingerprint"
        );
    }

    /// An aspect with no source at all has no fingerprint, so the caller falls
    /// through to a real build rather than suppressing on an empty hash — two
    /// different missing aspects must not share one negative-cache entry.
    #[test]
    fn an_empty_tree_has_no_fingerprint() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(fingerprint_dirs(&[tmp.path().join("nope")]).is_none());
    }

    /// The decision the pipeline makes, over the fingerprints a real edit
    /// sequence produces: refuse the identical retry, allow the fix.
    ///
    /// This models `known_bad_build`'s contract without standing up a whole
    /// `Grip` (which needs a runtime, a database and a checkout).
    #[test]
    fn a_failed_tree_is_refused_until_it_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("agent");
        let wit = tmp.path().join("wit");
        write(&src, "src/lib.rs", "broken");
        write(&wit, "thetis.wit", "package thetis:grip;");
        let roots = vec![src.clone(), wit.clone()];

        // A build fails; the pipeline remembers the source that caused it.
        let failed_at = fingerprint_dirs(&roots).unwrap();
        let remembered = Some(failed_at.clone());

        // The watcher fires again with nothing changed — the spin. Refused.
        let now = fingerprint_dirs(&roots).unwrap();
        assert_eq!(
            remembered.as_deref(),
            Some(now.as_str()),
            "an unchanged tree must match the remembered failure and be skipped"
        );

        // The agent fixes the source. The fingerprint moves, so the guard no
        // longer matches and a real build happens.
        write(&src, "src/lib.rs", "fixed");
        let after_fix = fingerprint_dirs(&roots).unwrap();
        assert_ne!(
            remembered.as_deref(),
            Some(after_fix.as_str()),
            "a fixed tree must not be suppressed"
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A repo on `main` holding a `wit/thetis.wit`, plus a branch cut from it.
    /// Mirrors the real shape: trunk moves the contract, a conversation branch
    /// is left behind holding the old one.
    async fn trunk_and_branch(branch: &str) -> (TempDir, crate::gitctl::GitCtl) {
        let tmp = TempDir::new().unwrap();
        let git = crate::gitctl::GitCtl::new(tmp.path());
        git.run_raw(&["init", "-b", "main"]).await.unwrap();
        git.run_raw(&["config", "user.name", "test"]).await.unwrap();
        git.run_raw(&["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        fs::create_dir_all(tmp.path().join("wit")).unwrap();
        fs::write(tmp.path().join("wit/thetis.wit"), "variant e { a, b }\n").unwrap();
        git.add_all_and_commit("base").await.unwrap().unwrap();

        // The branch is cut here, before the contract moves.
        git.run_raw(&["branch", branch]).await.unwrap();

        // Trunk gains a case, exactly as the reasoning-delta commit did.
        fs::write(tmp.path().join("wit/thetis.wit"), "variant e { a, b, c }\n").unwrap();
        git.add_all_and_commit("add a case").await.unwrap().unwrap();

        git.run_raw(&["checkout", branch]).await.unwrap();
        (tmp, git)
    }

    fn contract_of(dir: &std::path::Path) -> String {
        wit_fingerprint(&fs::read_to_string(dir.join("wit/thetis.wit")).unwrap())
    }

    /// The two fingerprints must be computed identically or every comparison
    /// between them is meaningless.
    #[test]
    fn the_kernels_fingerprint_and_a_checkouts_use_one_algorithm() {
        let tmp = TempDir::new().unwrap();
        let wit = tmp.path().join("wit");
        fs::create_dir_all(&wit).unwrap();
        let body = "package thetis:grip;\n";
        fs::write(wit.join("thetis.wit"), body).unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.paths.wit = wit;
        assert_eq!(
            checkout_wit_fingerprint(&cfg).unwrap(),
            wit_fingerprint(body),
            "a checkout's contract must hash the way the kernel's does"
        );
    }

    /// The case that broke: a branch with nothing of its own, left behind by a
    /// trunk WIT change. It must come forward, and losslessly.
    #[tokio::test]
    async fn a_stale_branch_is_fast_forwarded_onto_trunk() {
        let (tmp, git) = trunk_and_branch("conv/stale").await;
        let before = contract_of(tmp.path());

        let behind = update_to_trunk(&git, "main").await.unwrap();

        assert_eq!(behind, 1, "one commit behind trunk");
        assert_ne!(
            contract_of(tmp.path()),
            before,
            "the contract must have moved"
        );
        assert_eq!(
            contract_of(tmp.path()),
            wit_fingerprint("variant e { a, b, c }\n"),
            "and it must be trunk's"
        );
        // A fast-forward, so the branch carries no merge commit of its own.
        let (ahead, behind) = git.ahead_behind("HEAD", "main").await.unwrap();
        assert_eq!((ahead, behind), (0, 0), "branch is exactly trunk");
    }

    /// A branch with its own work cannot fast-forward. It still has to come
    /// forward, and its work has to survive.
    #[tokio::test]
    async fn a_branch_with_its_own_commits_merges_and_keeps_them() {
        let (tmp, git) = trunk_and_branch("conv/busy").await;
        fs::write(tmp.path().join("mine.txt"), "my work\n").unwrap();
        git.add_all_and_commit("my work").await.unwrap().unwrap();

        update_to_trunk(&git, "main").await.unwrap();

        assert_eq!(
            contract_of(tmp.path()),
            wit_fingerprint("variant e { a, b, c }\n"),
            "the contract came forward"
        );
        assert!(
            tmp.path().join("mine.txt").is_file(),
            "and the work survived"
        );
    }

    /// Uncommitted edits would block the merge outright. They are checkpointed,
    /// not discarded — a worker starting up must never eat someone's work.
    #[tokio::test]
    async fn uncommitted_work_is_checkpointed_rather_than_blocking_or_lost() {
        let (tmp, git) = trunk_and_branch("conv/dirty").await;
        fs::write(tmp.path().join("scratch.txt"), "unsaved\n").unwrap();

        update_to_trunk(&git, "main").await.unwrap();

        assert!(
            !git.is_dirty().await.unwrap(),
            "the tree is clean afterwards"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("scratch.txt")).unwrap(),
            "unsaved\n",
            "the edit is still there, committed rather than dropped"
        );
    }

    /// A conflict at worker startup is the worst moment to hand somebody a
    /// half-merged tree. It must back out cleanly and say so.
    #[tokio::test]
    async fn a_conflicting_merge_is_abandoned_and_leaves_the_branch_usable() {
        let (tmp, git) = trunk_and_branch("conv/conflict").await;
        // Same file, different content: guaranteed to conflict with trunk.
        fs::write(tmp.path().join("wit/thetis.wit"), "variant e { a, b, z }\n").unwrap();
        git.add_all_and_commit("my own contract")
            .await
            .unwrap()
            .unwrap();
        let head_before = git.head().await.unwrap();

        let err = update_to_trunk(&git, "main").await.unwrap_err();

        assert!(
            format!("{err:#}").contains("conflict"),
            "the caller must be told why: {err:#}"
        );
        assert!(
            !git.merge_in_progress().await.unwrap(),
            "no merge may be left in progress"
        );
        assert_eq!(
            git.head().await.unwrap(),
            head_before,
            "the branch is exactly where it was found"
        );
        assert!(!git.is_dirty().await.unwrap(), "and its tree is clean");
    }

    /// The gateway case, and the whole point of the fix: a role with no git
    /// still has to *say* the contract disagrees. It used to fall through to
    /// `Unknown`, whose report is silent — so the one process positioned to
    /// announce a stale binary at startup announced nothing.
    #[tokio::test]
    async fn a_role_without_git_reports_the_mismatch_instead_of_going_quiet() {
        let check = reconcile_against("aaaaaaaaaaaaaaaa", Some("bbbbbbbbbbbbbbbb"), None).await;

        assert!(
            matches!(check, ContractCheck::Unrepairable { .. }),
            "a mismatch with no git is unrepairable, not unknown: {check:?}"
        );
        assert!(!check.is_sound(), "and nothing built against it can load");
    }

    /// The same role, agreeing: no git must not turn a healthy startup into a
    /// scary log line.
    #[tokio::test]
    async fn a_role_without_git_stays_quiet_when_the_contract_agrees() {
        let check = reconcile_against("aaaaaaaaaaaaaaaa", Some("aaaaaaaaaaaaaaaa"), None).await;

        assert!(matches!(check, ContractCheck::Match), "{check:?}");
        assert!(check.is_sound());
    }

    /// No `wit/thetis.wit` at all is genuinely nothing to check, and stays
    /// distinct from a mismatch nobody can repair.
    #[tokio::test]
    async fn an_unreadable_contract_is_unknown_and_not_treated_as_a_mismatch() {
        let check = reconcile_against("aaaaaaaaaaaaaaaa", None, None).await;

        assert!(matches!(check, ContractCheck::Unknown), "{check:?}");
        assert!(check.is_sound(), "unknown must not blame later failures");
    }
}
