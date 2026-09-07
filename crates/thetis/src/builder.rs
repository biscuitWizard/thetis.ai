//! Compiling guest source trees to WebAssembly components.
//!
//! One serialized queue drives every build, whether it was triggered by a human
//! editing a file or by the agent rewriting itself. Both paths therefore get the
//! same validation and the same versioning guarantees.
//!
//! Builds use the host toolchain (`cargo build --target wasm32-wasip2`), which
//! is fast but means build scripts and proc macros run with the orchestrator's
//! privileges. The dev-kit compensates by refusing agent writes to `Cargo.toml`,
//! `build.rs`, and `.cargo/` — dependencies stay a human decision.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::aspect::Aspect;

#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub success: bool,
    /// Compiler diagnostics, trimmed to something a model can actually read.
    pub stderr: String,
    pub duration: Duration,
    pub wasm_path: Option<PathBuf>,
}

impl BuildOutcome {
    pub fn failed(stderr: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: false,
            stderr: stderr.into(),
            duration,
            wasm_path: None,
        }
    }
}

/// How a build should treat the lockfile.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions {
    /// Let cargo update `Cargo.lock`.
    ///
    /// Required after a manifest change: `--locked` fails outright when the
    /// lockfile no longer matches the dependencies, so adding a crate would
    /// never build without this.
    pub refresh_lockfile: bool,
}

/// Serializes builds so a shared cargo target directory is never contended.
#[derive(Default)]
pub struct Builder {
    lock: Mutex<()>,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where cargo puts an aspect's component. Sharing one target directory across
    /// all guests means dependencies compile once, which is what keeps the
    /// agent's edit-compile-fix loop tight.
    pub fn wasm_path(cfg: &Config, aspect: &Aspect) -> PathBuf {
        cfg.build
            .target_dir
            .join(&cfg.build.target)
            .join(&cfg.build.profile)
            .join(cfg.aspect_wasm_filename(aspect))
    }

    pub async fn build(&self, cfg: &Config, aspect: &Aspect) -> Result<BuildOutcome> {
        self.build_with(cfg, aspect, BuildOptions::default()).await
    }

    pub async fn build_with(
        &self,
        cfg: &Config,
        aspect: &Aspect,
        opts: BuildOptions,
    ) -> Result<BuildOutcome> {
        let _guard = self.lock.lock().await;
        // The in-process mutex serializes this worker; the file lock
        // serializes the fleet. Every worker shares one cargo target
        // directory, and two cargo invocations racing on the same output
        // paths corrupt neither but can hand one worker the other's binary.
        let started = Instant::now();
        let Some(_flock) = FileLock::acquire(&cfg.build_lock_path(), cfg.build.timeout).await?
        else {
            let secs = cfg.build.timeout.as_secs();
            tracing::warn!(%aspect, secs, "gave up waiting for the fleet build lock");
            return Ok(BuildOutcome::failed(
                format!(
                    "another checkout has been building for over {secs}s and this build could \
                     not start. The previous revision is still serving; try again shortly."
                ),
                started.elapsed(),
            ));
        };

        let src = cfg.aspect_source_dir(aspect);
        if !src.join("Cargo.toml").is_file() {
            return Ok(BuildOutcome::failed(
                format!("no crate found at {}", src.display()),
                started.elapsed(),
            ));
        }

        // No source is touched here, and nothing may reintroduce that.
        //
        // This used to dirty `src/lib.rs` before every build to force a
        // re-link, because every checkout compiled into one shared target
        // directory and cargo only rewrites the output when *this* copy is
        // dirty — so a "Fresh" build could hand back whichever checkout linked
        // last. The fix for that is per-checkout target directories (a worker's
        // `target_dir` now resolves against its own worktree), which removes
        // the need entirely.
        //
        // The touch was also actively harmful: `open(write)` + `set_modified`
        // emits an inotify MODIFY, indistinguishable from a real edit, so the
        // file watcher rebuilt the aspect that had just been built, forever.
        // Green trees escaped only via the pipeline's hash-identical
        // short-circuit, and a failing tree spun cargo without limit.
        let mut cmd = tokio::process::Command::new(&cfg.build.command);
        cmd.current_dir(&src)
            .env("CARGO_TARGET_DIR", &cfg.build.target_dir)
            // Plain, parseable diagnostics: this text goes to a language model.
            .env("CARGO_TERM_COLOR", "never")
            .arg("build")
            .arg("--target")
            .arg(&cfg.build.target);

        // cargo spells the default profile `--release`, but any other profile
        // is named with `--profile`.
        if cfg.build.profile == "release" {
            cmd.arg("--release");
        } else if cfg.build.profile != "debug" {
            cmd.arg("--profile").arg(&cfg.build.profile);
        }

        // `--locked` keeps dependency resolution reproducible, but only once a
        // lockfile exists; a freshly scaffolded tool has to generate one first.
        if cfg.build.locked && !opts.refresh_lockfile && src.join("Cargo.lock").is_file() {
            cmd.arg("--locked");
        }
        cmd.args(&cfg.build.extra_args);

        // The helper sets `kill_on_drop`, which is what makes the timeout below
        // actually release the lock rather than leak a running cargo that goes
        // on holding the target directory.
        let output = match crate::control::run_child_within(cmd, cfg.build.timeout)
            .await
            .with_context(|| format!("running cargo for {aspect}"))?
        {
            Some(output) => output,
            None => {
                let secs = cfg.build.timeout.as_secs();
                tracing::warn!(%aspect, secs, "build exceeded its timeout and was killed");
                return Ok(BuildOutcome::failed(
                    format!(
                        "the build ran longer than {secs}s and was stopped. This usually means a                          dependency could not be fetched. The previous revision is still serving."
                    ),
                    started.elapsed(),
                ));
            }
        };

        let duration = started.elapsed();
        let stderr = trim_diagnostics(&String::from_utf8_lossy(&output.stderr));

        if !output.status.success() {
            return Ok(BuildOutcome::failed(stderr, duration));
        }

        let wasm = Self::wasm_path(cfg, aspect);
        if !wasm.is_file() {
            return Ok(BuildOutcome::failed(
                format!(
                    "cargo reported success but no component appeared at {}\n{stderr}",
                    wasm.display()
                ),
                duration,
            ));
        }

        Ok(BuildOutcome {
            success: true,
            stderr,
            duration,
            wasm_path: Some(wasm),
        })
    }
}

/// An advisory cross-process lock, held for as long as the value lives.
///
/// `flock` blocks, so acquisition runs on the blocking pool; release is the
/// kernel dropping the lock when the file closes, which makes a crashed
/// holder impossible to deadlock on.
pub struct FileLock {
    _file: std::fs::File,
}

impl FileLock {
    /// Takes the fleet build lock, or gives up after `limit`.
    ///
    /// `Ok(None)` means someone else is building. This polls `LOCK_NB` rather
    /// than blocking in `LOCK_EX` for two reasons: a blocking `flock` parks a
    /// thread of the blocking pool per queued worker, and — worse — it has no
    /// deadline at all, so a worker waiting here was unreachable for as long
    /// as the holder took, with its stop button dead and the gateway's
    /// readiness timer running.
    pub async fn acquire(path: &std::path::Path, limit: Duration) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening build lock {}", path.display()))?;

        use std::os::fd::AsRawFd;
        let deadline = Instant::now() + limit;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                // Who is holding it, for whoever has to diagnose a stall.
                let _ = std::fs::write(path, format!("{}\n", std::process::id()));
                return Ok(Some(Self { _file: file }));
            }
            let err = std::io::Error::last_os_error();
            if !matches!(err.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                return Err(err).context("taking the build lock");
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Cargo is chatty. Keep the errors and drop the progress noise, then cap the
/// result so a pathological build cannot flood the model's context.
fn trim_diagnostics(raw: &str) -> String {
    const MAX: usize = 12_000;

    let kept: Vec<&str> = raw
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("Compiling")
                || t.starts_with("Downloaded")
                || t.starts_with("Downloading")
                || t.starts_with("Updating")
                || t.starts_with("Finished")
                || t.starts_with("Locking")
                || t.starts_with("Adding"))
        })
        .collect();

    let text = kept.join("\n").trim().to_string();
    if text.len() <= MAX {
        return text;
    }

    // Keep the head (the first errors are the ones worth fixing) and the tail
    // (which carries the summary line).
    let head: String = text.chars().take(MAX * 2 / 3).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(MAX / 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n\n[... diagnostics truncated ...]\n\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_drop_progress_noise_but_keep_errors() {
        let raw = "   Compiling foo v0.1.0\n\
                   error[E0425]: cannot find value `x`\n\
                    --> src/lib.rs:3:5\n\
                       Finished release profile\n";
        let out = trim_diagnostics(raw);
        assert!(out.contains("E0425"));
        assert!(out.contains("src/lib.rs:3:5"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Finished"));
    }

    #[test]
    fn diagnostics_are_capped() {
        let raw = "error: boom\n".repeat(4000);
        let out = trim_diagnostics(&raw);
        assert!(out.len() < 14_000, "was {}", out.len());
        assert!(out.contains("truncated"));
    }
}
