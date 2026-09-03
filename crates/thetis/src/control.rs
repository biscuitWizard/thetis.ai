//! Control over the orchestrator process itself.
//!
//! Restarting is how a change to the native binary — or to configuration read
//! only at startup — takes effect. Guest code cannot be trusted to do this
//! sensibly on its own, so it is rate limited by uptime and can be turned off.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::grip::Grip;

/// How long a freshly built kernel gets to answer `--probe`. A binary that
/// hangs here is exactly as unusable as one that fails, so it is killed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// When this process started, for the minimum-uptime guard.
static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// A copy of the binary this process is executing, filed somewhere a rebuild
/// will not touch. Empty until [`pin_self_exe`] runs, and on any process that
/// never calls it.
static SELF_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The path this process was launched from, as a name that can actually be
/// opened.
///
/// Linux answers `/proc/self/exe` for an unlinked binary with the original path
/// plus a literal " (deleted)" suffix, and `current_exe` hands that through
/// verbatim. Every rebuild unlinks the file the running kernel came from, so a
/// long-lived gateway ends up holding one of these — a name no file has ever
/// had. Taken literally it turns `spawn` into ENOENT and makes any comparison
/// against a build path fail, which is exactly the pair of ways a self-deploy
/// used to strand the system.
///
/// The suffix is only stripped when doing so actually finds a file, so a
/// binary genuinely named `... (deleted)` is left alone.
pub fn launch_path() -> Option<PathBuf> {
    resolve_exe(std::env::current_exe().ok()?, |p| p.is_file())
}

fn resolve_exe(raw: PathBuf, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    if exists(&raw) {
        return Some(raw);
    }
    let stripped = raw.as_os_str().as_bytes().strip_suffix(b" (deleted)")?;
    let stripped = PathBuf::from(std::ffi::OsString::from_vec(stripped.to_vec()));
    exists(&stripped).then_some(stripped)
}

/// Pins this process's own binary to a path a later build cannot pull away.
///
/// The gateway spawns every worker from its own executable, so it needs that
/// executable to still be there minutes or hours after it started — and a
/// merge that rebuilds trunk deletes it out from under us. Call this before
/// anything can rebuild, i.e. at startup.
///
/// Failure is not fatal: [`self_exe`] falls back to the launch path, which is
/// what the code did before this existed.
pub fn pin_self_exe(cfg: &crate::config::Config) {
    match pin(cfg) {
        Ok(path) => {
            tracing::debug!(pinned = %path.display(), "our own binary is pinned");
            let _ = SELF_EXE.set(path);
        }
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            "could not pin our own binary; a rebuild during this process's life may leave              new conversations unable to start a worker"
        ),
    }
}

fn pin(cfg: &crate::config::Config) -> Result<PathBuf> {
    let exe = launch_path().context("locating our own binary")?;
    pin_into(&exe, &cfg.paths.artifacts.join("cache/self"))
}

fn pin_into(exe: &Path, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let pinned = dir.join("thetis");
    let staging = dir.join(format!("thetis.incoming.{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);

    // A hard link costs no space and pins the *inode*: when a rebuild unlinks
    // and replaces the file, this entry still names the bytes we are actually
    // running, so a worker spawned from it is the same build as the gateway
    // that spawned it — and speaks the same IPC protocol. Only a separate
    // filesystem forces a real copy.
    if let Err(e) = std::fs::hard_link(exe, &staging) {
        tracing::debug!(error = %e, "no hard link available; copying instead");
        std::fs::copy(exe, &staging)
            .with_context(|| format!("copying our binary to {}", staging.display()))?;
    }

    // Rename into place rather than write over: the previous generation's
    // pinned file may still be executing, and writing onto a running binary
    // fails with ETXTBSY. A rename swaps the directory entry and leaves that
    // process holding the inode it started from.
    if let Err(e) = std::fs::rename(&staging, &pinned) {
        let _ = std::fs::remove_file(&staging);
        return Err(e).with_context(|| format!("pinning our binary at {}", pinned.display()));
    }
    Ok(pinned)
}

/// A path to *this exact build*, which still exists.
///
/// What to spawn a worker from: it has to be the gateway's own build, because
/// the two ends handshake on an IPC protocol version compiled into both.
pub fn self_exe() -> Result<PathBuf> {
    if let Some(pinned) = SELF_EXE.get() {
        if pinned.is_file() {
            return Ok(pinned.clone());
        }
        tracing::warn!(pinned = %pinned.display(), "the pinned binary is gone; falling back");
    }
    launch_path().context(
        "locating our own binary: the file this process was started from has been deleted          and no pinned copy survived it",
    )
}

/// Where a restart should exec from.
///
/// The launch path first, and deliberately so: a restart is how a *rebuilt*
/// binary at that path takes effect, and the pinned copy is by construction the
/// build being replaced. Restarting onto the pin would come back on the old
/// code and report success. The pin is only the fallback for when the launch
/// path is gone altogether, where coming back on the old code still beats not
/// coming back.
pub fn restart_exe() -> Result<PathBuf> {
    if let Some(exe) = launch_path() {
        return Ok(exe);
    }
    self_exe()
}

/// Everything the orchestrator binary is compiled from. A merge that moves any
/// of these leaves a running kernel behind the tree it is supposed to be a
/// build of — on a branch *and* on trunk.
pub const KERNEL_PATHS: [&str; 4] = ["crates", "wit", "Cargo.toml", "Cargo.lock"];

/// Where a checkout's own kernel build lands, and where a restart looks for it.
pub fn kernel_binary(root: &Path) -> PathBuf {
    root.join("target/release/thetis")
}

/// Did any of the kernel's source paths change between two revisions?
///
/// Errors read as "no": a missing tree is not evidence of a change, and
/// rebuilding the kernel on a bad answer is far more disruptive than skipping.
pub async fn kernel_source_moved(
    git: &crate::gitctl::GitCtl,
    before: &str,
    after: &str,
) -> bool {
    for path in KERNEL_PATHS {
        let was = git.tree_oid(before, path).await.ok().flatten();
        let now = git.tree_oid(after, path).await.ok().flatten();
        if was != now {
            return true;
        }
    }
    false
}

/// Only one kernel build at a time in this process. Two `cargo build` runs on
/// one target directory block on cargo's own lock anyway; this makes the second
/// caller skip instead of waiting out a multi-minute build.
///
/// This is necessary but not sufficient: every conversation runs in its own
/// worker *process*, so this mutex is uncontended while N workers each start a
/// release build of the orchestrator. On a four-core machine two were enough to
/// starve every conversation on the box. [`build_kernel`] therefore also takes
/// a file lock, which is what actually serializes the fleet.
static KERNEL_BUILD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How a kernel build ended.
///
/// [`KernelBuild::Busy`] is the case this type exists for. Declining because
/// someone else is already building is not a build failure, and reporting it as
/// one is actively misleading: a second `restart_orchestrator` in one process
/// used to be answered with "the orchestrator did not build, so nothing was
/// restarted ... fix this and ask again", moments before the *first* build
/// finished and restarted the process. The agent was told to fix a healthy
/// system and to repeat the request that had collided. Contention and failure
/// are different answers and now have different shapes.
#[derive(Debug)]
pub enum KernelBuild {
    /// Compiled, or served from the build cache. The binary a restart uses.
    Built(PathBuf),
    /// A build is already running — in this process, or elsewhere in the
    /// fleet. Nothing was compiled, nothing is wrong, and the right response
    /// is to wait for the one already going rather than to ask again.
    Busy(String),
}

/// Runs a child to completion, killing it if it outstays `limit`.
///
/// `Ok(None)` means it was killed. Every child this process spawns goes
/// through here (or through [`run_child`]), because `tokio::time::timeout`
/// alone does *not* stop the process: it drops the future, and unless the
/// command was built with `kill_on_drop` the child keeps running — holding the
/// build lock, the cargo target directory, and its pipes — while the caller
/// reports a clean timeout. Three of the four copies of this shape got that
/// wrong, one of them under a comment asserting the opposite.
pub async fn run_child_within(
    mut cmd: tokio::process::Command,
    limit: Duration,
) -> Result<Option<std::process::Output>> {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(result) => Ok(Some(result?)),
        Err(_) => Ok(None),
    }
}

/// [`run_child_within`], with the timeout reported as an error.
pub async fn run_child(
    cmd: tokio::process::Command,
    limit: Duration,
    what: &str,
) -> Result<std::process::Output> {
    match run_child_within(cmd, limit)
        .await
        .with_context(|| format!("running {what}"))?
    {
        Some(output) => Ok(output),
        None => bail!("{what} exceeded {}s and was killed", limit.as_secs()),
    }
}

/// The cache key for a kernel built from a given revision, or `None` when the
/// revision cannot be resolved.
///
/// Keyed on the trees in [`KERNEL_PATHS`] — everything the binary is compiled
/// from. Two checkouts whose four oids agree produce the same executable, so
/// they can share one, and a branch that has not touched `crates/` or `wit/`
/// never needs its own build at all.
///
/// The toolchain is part of the key too: the same source under a different
/// rustc is a different binary, and serving a stale one would be silent.
pub async fn kernel_cache_key(
    git: &crate::gitctl::GitCtl,
    rev: &str,
    toolchain: &str,
) -> Option<String> {
    let mut parts = vec![toolchain.to_string()];
    for path in KERNEL_PATHS {
        parts.push(git.tree_oid(rev, path).await.ok().flatten()?);
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    Some(crate::buildcache::BuildCache::cache_key(&refs))
}

/// Identifies the compiler, for the kernel cache key.
async fn toolchain_id(cfg: &crate::config::Config) -> String {
    let mut cmd = tokio::process::Command::new(&cfg.build.command);
    cmd.arg("--version");
    match run_child(cmd, PROBE_TIMEOUT, "cargo --version").await {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        // Unknown is a distinct, never-repeating key: better to rebuild than
        // to share a binary built by a compiler we cannot name.
        Err(_) => format!("unknown-toolchain-{}", crate::buildcache::now_ms()),
    }
}

/// Compiles the orchestrator from `cfg.root`, where a restart looks for it.
///
/// Returns the binary's path. Serialized process-wide, and it declines rather
/// than queues when another build already holds the lock.
///
/// A cached binary built from the same source by any checkout is used instead
/// of compiling. That is the difference between a restart costing seconds and
/// costing several minutes of every core: the ~390 dependency units behind the
/// orchestrator (cranelift and wasmtime dominate) are identical for every
/// branch that has not touched `crates/` or `wit/`.
pub async fn build_kernel(cfg: &crate::config::Config) -> Result<KernelBuild> {
    let Ok(_one_at_a_time) = KERNEL_BUILD.try_lock() else {
        return Ok(KernelBuild::Busy(
            "a kernel build is already running in this conversation".to_string(),
        ));
    };

    // And once across the fleet. A kernel build is the most expensive thing
    // this system does — minutes of every core, and its own multi-gigabyte
    // target directory per checkout — so several at once do not merely queue,
    // they starve the machine that is also serving conversations. Waiting is
    // the right behaviour here rather than declining: the caller has already
    // told the user their kernel is being rebuilt, and a build that is slow
    // because another finished first is still the build they asked for.
    let lock_path = cfg.kernel_build_lock_path();
    let Some(_fleet) = crate::builder::FileLock::acquire(&lock_path, KERNEL_BUILD_WAIT).await?
    else {
        return Ok(KernelBuild::Busy(format!(
            "another conversation has been building the orchestrator for over {}s",
            KERNEL_BUILD_WAIT.as_secs()
        )));
    };

    let destination = kernel_binary(&cfg.root);
    let cache = crate::buildcache::BuildCache::new(cfg.paths.artifacts.join("cache"));

    // Only a clean tree may use or fill the cache. With uncommitted edits the
    // key names HEAD, which is not what would be compiled — serving that would
    // quietly run code the source does not say, and storing under it would
    // poison the entry for every other checkout.
    let git = crate::gitctl::GitCtl::new(&cfg.root);
    let cache_key = if git.is_dirty().await.unwrap_or(true) {
        // Dirty, or not a repository at all: build, and do not cache.
        None
    } else {
        let toolchain = toolchain_id(cfg).await;
        kernel_cache_key(&git, "HEAD", &toolchain).await
    };

    if let Some(key) = &cache_key {
        if let Ok(Some(meta)) = cache.lookup(KERNEL_CACHE_ASPECT, key) {
            match cache.artifact_path(&meta, KERNEL_CACHE_ARTIFACT) {
                Ok(cached) => match install_kernel_from_cache(&cached, &destination).await {
                    Ok(()) => {
                        tracing::info!(
                            key = %key,
                            "kernel served from the build cache; no compile needed"
                        );
                        return Ok(KernelBuild::Built(destination));
                    }
                    // Not fatal: fall through and build properly.
                    Err(e) => tracing::warn!(
                        error = %format!("{e:#}"),
                        "a cached kernel would not install; building instead"
                    ),
                },
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "the cached kernel failed its integrity check; building instead"
                ),
            }
        }
    }

    let target = cfg.root.join("target");

    let mut cmd = tokio::process::Command::new(&cfg.build.command);
    cmd.current_dir(&cfg.root)
        // Pinned rather than inherited: a restart reads the binary from this
        // exact path, so the build has to put it there.
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_TERM_COLOR", "never")
        .args(["build", "--release", "-p", "thetis"]);

    let output = run_child(cmd, cfg.build.timeout, "the kernel build").await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        bail!(
            "cargo failed:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }

    // File it so the next checkout of this source does not pay again. Probed
    // first: an unprobed binary in the cache would be handed to other
    // conversations as if it were known good.
    if let Some(key) = &cache_key {
        match probe_kernel(&destination).await {
            Ok(()) => {
                if let Err(e) = store_kernel(&cache, key, &destination) {
                    tracing::warn!(error = %format!("{e:#}"), "could not cache the kernel build");
                }
            }
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "the fresh kernel did not pass its probe, so it was not cached"
            ),
        }
    }

    Ok(KernelBuild::Built(destination))
}

/// Where source-keyed kernel builds are filed, and under what name.
///
/// Deliberately a subdirectory of `cache/kernel`, not `cache/kernel` itself:
/// `workers::adopt_branch_kernel` already keeps binaries there keyed by
/// *commit*, with no `meta.json`. The two schemes answer different questions —
/// "which kernel did this branch adopt" versus "has this source been compiled
/// by anyone" — and a commit key rebuilds whenever any file in the repo
/// changes, which is the cost this cache exists to avoid. Keeping them in
/// separate directories means neither can read the other's entries by
/// accident.
const KERNEL_CACHE_ASPECT: &str = "kernel/by-source";
const KERNEL_CACHE_ARTIFACT: &str = "thetis";

/// Copies a cached kernel into the place a restart reads from.
///
/// Staged and renamed rather than written in place: the destination may be a
/// running binary, and writing onto one fails with ETXTBSY.
async fn install_kernel_from_cache(cached: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let staging = destination.with_extension(format!("incoming.{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    std::fs::copy(cached, &staging)
        .with_context(|| format!("copying the cached kernel to {}", staging.display()))?;

    // The copy loses the executable bit on some filesystems; a restart needs it.
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&staging)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&staging, perms)?;

    // Confirm it runs *before* it becomes the binary a restart will exec.
    if let Err(e) = probe_kernel(&staging).await {
        let _ = std::fs::remove_file(&staging);
        return Err(e).context("the cached kernel failed its probe");
    }

    std::fs::rename(&staging, destination).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        e
    })?;
    Ok(())
}

fn store_kernel(
    cache: &crate::buildcache::BuildCache,
    key: &str,
    binary: &Path,
) -> Result<()> {
    let meta = crate::buildcache::BuildMeta {
        aspect: KERNEL_CACHE_ASPECT.to_string(),
        key: key.to_string(),
        artifact_sha256: crate::buildcache::hash_file(binary)?,
        smoke: crate::buildcache::SmokeVerdict::Pass,
        source_commit: String::new(),
        aspect_tree: String::new(),
        wit_tree: String::new(),
        created_ms: crate::buildcache::now_ms(),
        note: "orchestrator build".to_string(),
    };
    cache.store(binary, KERNEL_CACHE_ARTIFACT, &meta)?;
    Ok(())
}

/// Asks a kernel binary whether it starts and speaks our IPC protocol.
///
/// The gate before *any* process is replaced by a fresh build: a binary that
/// cannot answer this would take its conversation — or, on trunk, the whole
/// system — down with it.
pub async fn probe_kernel(kernel: &Path) -> Result<()> {
    if !kernel.is_file() {
        bail!("{} does not exist", kernel.display());
    }
    let mut cmd = tokio::process::Command::new(kernel);
    cmd.arg("worker").arg("--probe").env_remove("INVOCATION_ID");
    let probe = run_child(cmd, PROBE_TIMEOUT, "the kernel probe").await?;

    let answer = String::from_utf8_lossy(&probe.stdout);
    let expected = format!("thetis-worker-probe-ok {}", crate::ipc::PROTOCOL_VERSION);
    if !probe.status.success() || !answer.contains(&expected) {
        bail!(
            "the probe answered {:?} (wanted '{expected}') — an incompatible or broken build",
            answer.trim()
        );
    }
    Ok(())
}

pub fn mark_start() {
    let _ = STARTED.set(Instant::now());
}

pub fn uptime() -> Duration {
    STARTED.get().map(|t| t.elapsed()).unwrap_or_default()
}

/// Free space, in bytes, on the filesystem holding `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// A release build of the orchestrator needs room for its own target
/// directory, and every conversation that builds one has its own.
///
/// Checked before starting rather than discovered half way: a build that runs
/// the disk out does not just fail, it takes the database and every other
/// conversation's build with it. Refusing early is recoverable; a full disk on
/// a machine that is also serving is not.
const KERNEL_BUILD_HEADROOM: u64 = 20 * 1024 * 1024 * 1024;

/// How long to wait for another checkout's kernel build before giving up.
///
/// Generous, because the thing being waited for legitimately takes minutes and
/// the alternative is refusing a restart the user asked for. Bounded, because
/// a crashed holder must not wedge every future kernel build — though `flock`
/// releases on process death, so that is a backstop rather than the mechanism.
const KERNEL_BUILD_WAIT: Duration = Duration::from_secs(20 * 60);

fn refuse_if_disk_is_tight(cfg: &crate::config::Config) -> Result<()> {
    let Some(free) = free_bytes(&cfg.root) else {
        return Ok(()); // cannot tell; do not block on a failed syscall
    };
    if free >= KERNEL_BUILD_HEADROOM {
        return Ok(());
    }
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    bail!(
        "not enough disk to rebuild the orchestrator safely: {:.1} GiB free, and a release \
         build of it needs room for its own target directory (this conversation has one, and \
         so does every other). Nothing was built or restarted. Ask the user to reclaim space \
         — `cargo clean` in an idle checkout is usually the quickest.",
        gib(free)
    )
}

/// Whether the kernel binary in this checkout is older than the source it is
/// built from.
///
/// Compared by modification time rather than anything cleverer: the question
/// is only "has the agent edited the orchestrator since this binary was
/// built", and a false positive costs one build that the cache mostly serves
/// anyway.
/// Closes the shells and asks whoever owns the process tree to bounce us.
async fn finish_restart(grip: &Arc<Grip>, reason: String) {
    let delay = grip.cfg.control.restart_delay;
    tracing::warn!(reason = %reason, "restart requested; the process will replace itself shortly");

    let grip = grip.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        // Shells outlive the process otherwise, and would hold their pipes open.
        grip.terminals.close_all().await;

        match &grip.role {
            crate::grip::Role::Worker(peer) => {
                // A worker restart touches nobody else. If this branch has
                // built its own kernel, the gateway probes it and respawns
                // the worker on it; otherwise the worker just comes back on
                // the binary it was started with.
                let built = grip.cfg.root.join("target/release/thetis");
                let kernel = built.is_file().then(|| built.display().to_string());
                peer.notify(
                    "restart_worker",
                    serde_json::json!({ "reason": reason, "kernel": kernel }),
                )
                .await;
                // The gateway takes it from here: probe, cache, shutdown,
                // respawn, resume.
            }
            crate::grip::Role::Gateway(_) => respawn_process(),
        }
    });
}

fn kernel_is_stale(cfg: &crate::config::Config) -> bool {
    let built = kernel_binary(&cfg.root);
    let Ok(built_at) = std::fs::metadata(&built).and_then(|m| m.modified()) else {
        return true; // never built here
    };
    let mut newest = std::time::SystemTime::UNIX_EPOCH;
    let mut stack = vec![cfg.root.join("crates"), cfg.root.join("wit")];
    for f in ["Cargo.toml", "Cargo.lock"] {
        if let Ok(m) = std::fs::metadata(cfg.root.join(f)).and_then(|m| m.modified()) {
            newest = newest.max(m);
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                    continue;
                }
                stack.push(path);
            } else if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                newest = newest.max(m);
            }
        }
    }
    newest > built_at
}

/// Set while a build-then-restart is in flight in this process.
///
/// One restart request is one restart. Without this, a second
/// `restart_orchestrator` while the first was still compiling spawned a second
/// build that could only collide with it — and the collision was reported as a
/// build failure telling the agent to fix something and ask again, seconds
/// before the first build succeeded and restarted the process. Two
/// contradictory verdicts for one request, the wrong one first.
static RESTART_BUILDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Clears the flag it claimed however the build ends, panic included. A flag
/// left set would make the process unrestartable, which is a worse failure
/// than the double-request it exists to prevent.
struct RestartBuildGuard(&'static std::sync::atomic::AtomicBool);

impl Drop for RestartBuildGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Claims the right to build-and-restart, or `None` if one is already going.
fn claim_restart_build() -> Option<RestartBuildGuard> {
    claim_flag(&RESTART_BUILDING)
}

/// The claim itself, over whichever flag. Parameterised so a test can use its
/// own rather than the process-wide one, which two tests sharing would race.
fn claim_flag(flag: &'static std::sync::atomic::AtomicBool) -> Option<RestartBuildGuard> {
    flag.compare_exchange(
        false,
        true,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    )
    .ok()
    .map(|_| RestartBuildGuard(flag))
}

/// Builds this checkout's kernel, then restarts onto it.
///
/// Spawned, because a release build takes minutes while the call that asked
/// for it must return in seconds — so the verdict arrives in the session log
/// instead of as a return value. A build that fails does *not* restart: the
/// conversation keeps the process it has and reads the compiler error.
///
/// The guard is held for the whole spawned task, so a restart request that
/// arrives while this one is compiling is answered rather than raced.
fn build_then_restart(
    grip: Arc<Grip>,
    reason: String,
    session_id: String,
    guard: RestartBuildGuard,
) {
    tokio::spawn(async move {
        let _building = guard;
        let note = |text: String| {
            let grip = grip.clone();
            let session = session_id.clone();
            async move {
                let _ = grip
                    .append_event(&session, crate::bindings::types::SessionEvent::Incident(text))
                    .await;
            }
        };

        if let Err(e) = refuse_if_disk_is_tight(&grip.cfg) {
            note(format!("{e:#}")).await;
            return;
        }

        tracing::info!(session = %session_id, "building this branch's kernel before restarting");
        match build_kernel(&grip.cfg).await {
            Ok(KernelBuild::Built(path)) => {
                if let Err(e) = probe_kernel(&path).await {
                    note(format!(
                        "The orchestrator built, but the new binary would not start, so nothing \
                         was restarted and you are still on the previous one: {e:#}"
                    ))
                    .await;
                    return;
                }
                note(
                    "The orchestrator rebuilt from your changes and passed its startup probe. \
                     Restarting onto it now."
                        .to_string(),
                )
                .await;
                finish_restart(&grip, reason).await;
            }
            // Not a failure, and emphatically not something to ask again
            // about: the build that is already running is the one that was
            // asked for, and it will announce its own verdict here.
            Ok(KernelBuild::Busy(why)) => {
                note(format!(
                    "A rebuild of the orchestrator is already under way ({why}), so this request \
                     did not start a second one. Wait for it: its result arrives in this \
                     conversation, and if it succeeds the restart happens on its own. There is \
                     nothing to fix and nothing to ask again."
                ))
                .await;
            }
            Err(e) => {
                note(format!(
                    "The orchestrator did not build, so nothing was restarted and you are still \
                     on the previous one. Fix this and ask again:\n\n{e:#}"
                ))
                .await;
            }
        }
    });
}

/// Schedules a restart and returns immediately.
///
/// The delay matters: the call has to return so the turn can finish and the
/// user can read why the process is about to go away. Restarting inside the
/// call would kill the turn mid-sentence and leave no explanation.
///
/// From a worker, "restart" means *this worker alone* — no other
/// conversation notices. The worker records the resume preference, closes
/// its shells, and asks the gateway (which owns the process tree) to bounce
/// it; if the branch has built its own kernel, the gateway probes that
/// binary and brings the worker back on it.
pub async fn request_restart(
    grip: &Arc<Grip>,
    reason: &str,
    resume: bool,
    session_id: Option<&str>,
) -> Result<String> {
    let cfg = &grip.cfg;

    if !cfg.control.allow_restart {
        return Err(anyhow!(
            "restarting is off; set control.allow_restart in thetis.toml to turn it on"
        ));
    }

    // The uptime guard is for the gateway, whose restart loop would take the
    // whole system with it. A worker restarting young is normal — it may have
    // been materialized seconds ago precisely to apply a change — and its
    // crash loops are already handled by the gateway's fast-death ledger.
    if matches!(grip.role, crate::grip::Role::Gateway(_)) {
        let up = uptime();
        if up < cfg.control.min_uptime {
            return Err(anyhow!(
                "this process has only been up {:.0}s; restarts are refused before {:.0}s so a \
                 failing restart cannot become a loop",
                up.as_secs_f64(),
                cfg.control.min_uptime.as_secs_f64()
            ));
        }
    }

    // Recorded before the process goes away: on the way back up, startup
    // reconciliation reads this to decide whether to carry the turn on.
    if let Some(session) = session_id {
        if let Err(e) = grip.persist.set_no_resume(session, !resume).await {
            tracing::warn!(error = %e, "could not record the resume preference");
        }
        if resume {
            // This interruption is ours, so it must not be charged against the
            // turn's resume budget. Adopting a kernel you just built means
            // restarting mid-turn, and an agent doing that twice would
            // otherwise have its turn abandoned for working as intended.
            if let Err(e) = grip.persist.expect_restart(session).await {
                tracing::warn!(error = %e, "could not record the expected restart");
            }
        }
    }

    let reason = reason.trim().to_string();

    // The orchestrator's own source is the one thing the dev-kit cannot
    // rebuild, so an agent that edited `crates/` had no supported way to run
    // its change: it shelled out to cargo, hit the tool timeout, backgrounded
    // the build with `setsid` and polled a log file — which is where orphaned
    // builds came from. Restarting *is* the moment that build is needed, so
    // this is where it belongs.
    if let (Some(session), true) = (session_id, kernel_is_stale(cfg)) {
        if matches!(grip.role, crate::grip::Role::Worker(_)) {
            // Asking twice is not asking for two of them. A second request
            // while the first is still compiling is answered here, with the
            // truth, rather than becoming a second build that can only lose a
            // race with the first and be reported as its failure.
            let Some(guard) = claim_restart_build() else {
                return Ok(
                    "a rebuild of the orchestrator is already running for this conversation, so \
                     this request did not start another. It is the same build you asked for: its \
                     verdict arrives here, and a successful one restarts on its own. Carry on \
                     with something else, and do not ask again — repeating the request cannot \
                     make it finish sooner."
                        .to_string(),
                );
            };
            build_then_restart(grip.clone(), reason.clone(), session.to_string(), guard);
            return Ok(format!(
                "you have changed the orchestrator's own source, so it is being rebuilt before \
                 anything restarts: {reason}. This takes a few minutes and does not block you — \
                 the result arrives in this conversation, and a build that fails restarts \
                 nothing."
            ));
        }
    }

    let delay = cfg.control.restart_delay;
    finish_restart(grip, reason.clone()).await;

    Ok(format!(
        "restarting in {:.1}s: {reason}. {}",
        delay.as_secs_f64(),
        if resume {
            "This turn will carry on once Thetis is back, so there is no need to repeat yourself."
        } else {
            "This turn ends here."
        }
    ))
}

/// Hands over to a replacement process.
///
/// Under a supervisor, exiting *is* the restart: systemd sets INVOCATION_ID for
/// its units, `Restart=always` brings the service straight back, and the
/// replacement is supervised and journalled like any other start.
///
/// Spawning our own child there is actively harmful. systemd sees the main
/// process exit and considers the unit stopped; `KillMode=process` spares the
/// child on purpose, but nothing adopts it, so it is reparented to init -
/// unsupervised, its output going nowhere, and still holding the database. Every
/// later restart then fails with "Database already open", and `systemctl
/// restart` cannot reach the process actually serving traffic.
///
/// With no supervisor there is nothing to come back, so the process starts its
/// own replacement first and exits only once that has succeeded.
///
/// On any failure it logs and stays up: staying alive beats exiting into
/// nothing.
pub fn respawn_process() {
    if let Err(e) = respawn() {
        tracing::error!(error = %e, "restart failed; continuing to run");
    }
}

fn respawn() -> Result<()> {
    if std::env::var_os("INVOCATION_ID").is_some() {
        tracing::info!("under a supervisor; exiting for it to start the replacement");
        std::process::exit(0);
    }

    // Not `current_exe`: after a rebuild that is the unlinked path of the
    // binary we are replacing, and exec'ing it fails with ENOENT — the restart
    // would be refused at the last step, having already announced itself.
    let exe = restart_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    tracing::info!(exe = %exe.display(), "spawning replacement process");
    std::process::Command::new(&exe)
        .args(&args)
        .current_dir(std::env::current_dir()?)
        .spawn()
        .map_err(|e| anyhow!("cannot start {}: {e}", exe.display()))?;

    // The replacement retries binding, so it is fine that this process still
    // holds the port for a moment.
    tracing::info!("replacement started; exiting");
    std::process::exit(0);
}

#[cfg(test)]
mod kernel_cache_tests {
    use super::*;
    use crate::gitctl::GitCtl;

    fn git_cmd(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("running git");
        assert!(out.status.success(), "git {args:?} failed");
    }

    async fn repo(tmp: &std::path::Path) -> GitCtl {
        let git = GitCtl::new(tmp);
        git_cmd(tmp, &["init", "-b", "main"]);
        git_cmd(tmp, &["config", "user.name", "test"]);
        git_cmd(tmp, &["config", "user.email", "t@example.com"]);
        for path in ["crates/thetis", "wit"] {
            std::fs::create_dir_all(tmp.join(path)).unwrap();
        }
        std::fs::write(tmp.join("crates/thetis/lib.rs"), "fn main() {}").unwrap();
        std::fs::write(tmp.join("wit/thetis.wit"), "package thetis:grip;").unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(tmp.join("Cargo.lock"), "# lock").unwrap();
        git.add_all_and_commit("base").await.unwrap().unwrap();
        git
    }

    /// Sharing a kernel binary between checkouts is only safe if the key
    /// changes whenever the compiled result would. Each of the four
    /// KERNEL_PATHS, and the toolchain, must move it.
    #[tokio::test]
    async fn the_kernel_key_covers_every_input_to_the_binary() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = repo(tmp.path()).await;

        let base = kernel_cache_key(&git, "HEAD", "rustc 1.0")
            .await
            .expect("a committed tree has a key");

        // Deterministic: this is what lets a second checkout find the entry.
        assert_eq!(
            base,
            kernel_cache_key(&git, "HEAD", "rustc 1.0").await.unwrap()
        );

        // A different compiler is a different binary.
        assert_ne!(
            base,
            kernel_cache_key(&git, "HEAD", "rustc 2.0").await.unwrap(),
            "the toolchain must be part of the key"
        );

        // Each compiled-from path must move the key, or a real change to the
        // kernel would silently serve the previous binary.
        for (file, body) in [
            ("crates/thetis/lib.rs", "fn main() { changed() }"),
            ("wit/thetis.wit", "package thetis:grip; // changed"),
            ("Cargo.toml", "[workspace]\n# changed"),
            ("Cargo.lock", "# lock changed"),
        ] {
            let before = kernel_cache_key(&git, "HEAD", "rustc 1.0").await.unwrap();
            std::fs::write(tmp.path().join(file), body).unwrap();
            git.add_all_and_commit("change").await.unwrap().unwrap();
            let after = kernel_cache_key(&git, "HEAD", "rustc 1.0").await.unwrap();
            assert_ne!(before, after, "{file} must change the kernel cache key");
        }

        // A file the kernel is *not* built from must not change the key —
        // otherwise every branch gets its own entry and nothing is ever shared.
        let before = kernel_cache_key(&git, "HEAD", "rustc 1.0").await.unwrap();
        std::fs::write(tmp.path().join("README.md"), "docs").unwrap();
        git.add_all_and_commit("docs").await.unwrap().unwrap();
        assert_eq!(
            before,
            kernel_cache_key(&git, "HEAD", "rustc 1.0").await.unwrap(),
            "an unrelated file must not change the kernel cache key"
        );
    }

    /// Two separate checkouts of the same source must agree on the key: that
    /// agreement is the entire mechanism for skipping the build.
    #[tokio::test]
    async fn two_checkouts_of_one_tree_share_a_key() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        let git_a = repo(a.path()).await;
        let git_b = repo(b.path()).await;

        assert_eq!(
            kernel_cache_key(&git_a, "HEAD", "rustc 1.0").await.unwrap(),
            kernel_cache_key(&git_b, "HEAD", "rustc 1.0").await.unwrap(),
            "identical source in two checkouts must produce one key"
        );
    }

    /// An unresolvable revision yields no key, so the caller builds rather
    /// than sharing an entry under a meaningless name.
    #[tokio::test]
    async fn an_unknown_revision_has_no_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = repo(tmp.path()).await;
        assert!(kernel_cache_key(&git, "nope", "rustc 1.0").await.is_none());
    }

    /// The store-then-install round trip, over a stand-in "binary" that is a
    /// shell script so it can actually be probed.
    ///
    /// What this pins: the installed file is executable, byte-identical to
    /// what was stored, and lands at the path a restart reads
    /// (`target/release/thetis`).
    #[tokio::test]
    async fn a_stored_kernel_installs_as_an_executable_at_the_restart_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = crate::buildcache::BuildCache::new(tmp.path().join("cache"));

        // A "kernel" that answers the probe the way a real one does.
        let built = tmp.path().join("built-thetis");
        std::fs::write(
            &built,
            format!(
                "#!/bin/sh\necho 'thetis-worker-probe-ok {}'\n",
                crate::ipc::PROTOCOL_VERSION
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o755)).unwrap();

        store_kernel(&cache, "key-abc", &built).expect("storing");

        // A second checkout looks the key up and installs it.
        let meta = cache
            .lookup(KERNEL_CACHE_ASPECT, "key-abc")
            .unwrap()
            .expect("the entry is found by key");
        let cached = cache
            .artifact_path(&meta, KERNEL_CACHE_ARTIFACT)
            .expect("the artifact passes its integrity check");

        let destination = tmp.path().join("checkout-b/target/release/thetis");
        install_kernel_from_cache(&cached, &destination)
            .await
            .expect("installing");

        assert!(destination.is_file(), "the kernel lands at the restart path");
        assert_eq!(
            std::fs::read(&built).unwrap(),
            std::fs::read(&destination).unwrap(),
            "the installed kernel is byte-identical to the one built"
        );
        let mode = std::fs::metadata(&destination).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "the installed kernel must be executable");

        // Installing again over the file that is already there must work:
        // a retry at the same key is normal, and writing onto a running
        // binary is what the staging-and-rename dance exists to survive.
        install_kernel_from_cache(&cached, &destination)
            .await
            .expect("installing a second time");
    }

    /// A cached binary that does not answer the probe must never be renamed
    /// into the restart path: a restart onto it would take the conversation
    /// down, and the whole point of the cache is to be invisible when it works
    /// and harmless when it does not.
    #[tokio::test]
    async fn a_cached_kernel_that_fails_its_probe_is_not_installed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bad = tmp.path().join("bad-thetis");
        std::fs::write(&bad, "#!/bin/sh\necho nonsense\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();

        let destination = tmp.path().join("target/release/thetis");
        assert!(
            install_kernel_from_cache(&bad, &destination).await.is_err(),
            "a binary that fails the probe must be refused"
        );
        assert!(
            !destination.exists(),
            "nothing may be left at the restart path"
        );

        // And no staging litter behind.
        let strays: Vec<_> = std::fs::read_dir(tmp.path().join("target/release"))
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(strays.is_empty(), "staging files must be cleaned up: {strays:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One restart request is one restart. The second used to spawn a build
    // that could only collide with the first, and the collision was reported
    // as "the orchestrator did not build ... fix this and ask again" moments
    // before the first build finished and restarted the process.
    #[test]
    fn only_one_restart_build_is_claimed_at_a_time() {
        static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let first = claim_flag(&FLAG).expect("nothing is building yet");
        assert!(
            claim_flag(&FLAG).is_none(),
            "a second request must be answered, not raced"
        );
        drop(first);
        assert!(
            claim_flag(&FLAG).is_some(),
            "the claim has to be released when the build ends, or restarts stop working"
        );
    }

    // Whatever happens to the build — success, failure, panic — the next
    // request must be able to claim. A stuck flag would make the system
    // unrestartable, which is worse than the bug it fixes.
    #[test]
    fn a_panicking_build_still_releases_its_claim() {
        static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let _ = std::panic::catch_unwind(|| {
            let _guard = claim_flag(&FLAG).expect("free");
            panic!("the build blew up");
        });
        assert!(claim_flag(&FLAG).is_some());
    }

    // Contention is not failure. `build_kernel` returning `Err` here is what
    // made a healthy, already-running build read as a broken one.
    #[tokio::test]
    async fn a_build_that_collides_reports_busy_rather_than_an_error() {
        let cfg = crate::config::Config::load().expect("the shipped config loads");
        let _held = KERNEL_BUILD.lock().await;
        match build_kernel(&cfg).await {
            Ok(KernelBuild::Busy(why)) => {
                assert!(why.contains("already running"), "{why}");
            }
            other => panic!("a collision must not read as a build failure: {other:?}"),
        }
    }

    /// The "(deleted)" suffix Linux appends to `/proc/self/exe` after a
    /// rebuild. Read literally it names nothing, which is how a self-deploy
    /// came to both skip its own restart and break every new worker spawn.
    #[test]
    fn resolve_exe_sees_through_the_deleted_marker() {
        let real = PathBuf::from("/opt/thetis/thetis");
        let marked = PathBuf::from("/opt/thetis/thetis (deleted)");

        // The rebuild case: only the un-suffixed name is on disk.
        let only_real = |p: &Path| p == real;
        assert_eq!(resolve_exe(marked.clone(), only_real), Some(real.clone()));

        // An untouched process resolves to itself, marker or not.
        assert_eq!(resolve_exe(real.clone(), only_real), Some(real.clone()));

        // A file genuinely named "... (deleted)" is left alone: it exists, so
        // there is nothing to see through.
        let only_marked = |p: &Path| p == marked;
        assert_eq!(resolve_exe(marked.clone(), only_marked), Some(marked.clone()));

        // Nothing on disk under either name is an honest "we cannot say".
        assert_eq!(resolve_exe(marked, |_: &Path| false), None);
    }

    /// The property the whole fix rests on: once pinned, the binary stays
    /// reachable — and stays *this* build — across a rebuild that unlinks and
    /// replaces the file it was pinned from.
    #[test]
    fn a_pinned_binary_survives_the_rebuild_that_replaces_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let exe = tmp.path().join("thetis");
        std::fs::write(&exe, b"build one").unwrap();

        let pinned = pin_into(&exe, &tmp.path().join("cache/self")).unwrap();
        assert!(pinned.is_file());

        // What cargo does to the previous binary: unlink, then write a new
        // file at the same path.
        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, b"build two").unwrap();

        assert!(pinned.is_file(), "the pin did not survive the rebuild");
        assert_eq!(
            std::fs::read(&pinned).unwrap(),
            b"build one",
            "the pin must stay the build the running process came from, or a worker \
             spawned from it would not speak the gateway's protocol"
        );
    }

    /// Pinning again over a pin that a still-running process is executing.
    /// Writing onto a running binary fails with ETXTBSY, so the pin is renamed
    /// into place; the older generation keeps the inode it started from.
    #[test]
    fn re_pinning_replaces_the_entry_without_disturbing_the_old_inode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("cache/self");
        let exe = tmp.path().join("thetis");

        std::fs::write(&exe, b"build one").unwrap();
        let first = pin_into(&exe, &dir).unwrap();
        let held = std::fs::File::open(&first).unwrap();

        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, b"build two").unwrap();
        let second = pin_into(&exe, &dir).unwrap();

        assert_eq!(first, second, "the pin lives at one stable path");
        assert_eq!(std::fs::read(&second).unwrap(), b"build two");

        // The handle opened before the swap still reads the old build.
        use std::io::Read;
        let mut old = String::new();
        (&held).read_to_string(&mut old).unwrap();
        assert_eq!(old, "build one");

        // No staging files left lying around.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "thetis")
            .collect();
        assert!(strays.is_empty(), "left behind {strays:?}");
    }

    /// The predicate both merge directions gate their kernel rebuild on. Get
    /// this wrong in the "no" direction and new native code silently never
    /// runs; wrong in the "yes" direction and every merge pays for a build.
    #[tokio::test]
    async fn kernel_source_moved_sees_native_changes_and_ignores_guest_ones() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = crate::gitctl::GitCtl::new(tmp.path());
        // Plain git for setup: GitCtl deliberately exposes no escape hatch.
        for args in [
            ["init", "-b", "main"].as_slice(),
            &["config", "user.name", "test"],
            &["config", "user.email", "t@example.com"],
        ] {
            let out = std::process::Command::new("git")
                .current_dir(tmp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }

        let write = |rel: &str, text: &str| {
            let path = tmp.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };

        write("crates/thetis/src/lib.rs", "// v1\n");
        write("wit/thetis.wit", "// contract\n");
        write("Cargo.toml", "[workspace]\n");
        write("agents/agent-core/src/lib.rs", "// agent v1\n");
        let base = git.add_all_and_commit("base").await.unwrap().unwrap();

        // A guest-only change: hot-swappable, so no kernel rebuild.
        write("agents/agent-core/src/lib.rs", "// agent v2\n");
        let guest_only = git.add_all_and_commit("agent edit").await.unwrap().unwrap();
        assert!(!kernel_source_moved(&git, &base, &guest_only).await);

        // Native code: the kernel must be rebuilt.
        write("crates/thetis/src/lib.rs", "// v2\n");
        let native = git.add_all_and_commit("kernel edit").await.unwrap().unwrap();
        assert!(kernel_source_moved(&git, &guest_only, &native).await);

        // The contract counts too — it is compiled into the kernel.
        write("wit/thetis.wit", "// contract v2\n");
        let contract = git.add_all_and_commit("wit edit").await.unwrap().unwrap();
        assert!(kernel_source_moved(&git, &native, &contract).await);

        // And a manifest change, which moves the build itself.
        write("Cargo.toml", "[workspace]\n# tweak\n");
        let manifest = git.add_all_and_commit("manifest").await.unwrap().unwrap();
        assert!(kernel_source_moved(&git, &contract, &manifest).await);

        // A revision compared with itself never triggers a build.
        assert!(!kernel_source_moved(&git, &manifest, &manifest).await);
    }

    #[test]
    fn uptime_is_zero_until_marked() {
        // Nothing has called mark_start in this test binary, so the guard reads
        // as a brand new process — which is the conservative direction.
        assert!(uptime() < Duration::from_secs(1));
    }
}
