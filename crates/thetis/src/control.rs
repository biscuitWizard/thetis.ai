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
static KERNEL_BUILD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Compiles the orchestrator from `cfg.root`, where a restart looks for it.
///
/// Returns the binary's path. Serialized process-wide, and it declines rather
/// than queues when another build already holds the lock.
pub async fn build_kernel(cfg: &crate::config::Config) -> Result<PathBuf> {
    let Ok(_one_at_a_time) = KERNEL_BUILD.try_lock() else {
        bail!("a kernel build is already running in this process");
    };
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
    Ok(kernel_binary(&cfg.root))
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
    }

    let reason = reason.trim().to_string();
    let delay = cfg.control.restart_delay;
    tracing::warn!(reason = %reason, "restart requested; the process will replace itself shortly");

    let grip = grip.clone();
    let note_reason = reason.clone();
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
                    serde_json::json!({ "reason": note_reason, "kernel": kernel }),
                )
                .await;
                // The gateway takes it from here: probe, cache, shutdown,
                // respawn, resume.
            }
            crate::grip::Role::Gateway(_) => respawn_process(),
        }
    });

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
mod tests {
    use super::*;

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
