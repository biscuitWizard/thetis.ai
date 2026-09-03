//! The headless browser sidecar, and making sure Playwright is there to run.
//!
//! Tool components are `wasm32-wasip2`. They cannot spawn a process, so no tool
//! can drive Playwright itself — but every guest is linked against `wasi:http`,
//! and loopback is reachable. So the kernel owns one Node process running
//! Playwright headless, and the `web-browser-*` tools are thin HTTP clients to
//! it. One browser is shared by every conversation; each session id gets its own
//! `BrowserContext`, which is Playwright's isolation boundary.
//!
//! This module owns three things:
//!
//! * **Setup.** Checking node, installing the pinned Playwright into the
//!   vendored `package.json`, and confirming a browser is on disk — all
//!   idempotent, so a warm boot does no work and touches no network.
//! * **Supervision.** Starting the sidecar and restarting it if it dies.
//! * **A token.** Loopback alone would let any local process drive the browser,
//!   so the kernel generates a token at boot and only tells the tools.
//!
//! Headless is deliberately not configurable. A machine running Thetis has no
//! display, so a headed browser would hang waiting for one.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::config::{BrowserSettings, Config};

/// The token this process expects on every sidecar request. Generated once at
/// boot; handed to tools through their config block and never written to disk.
static TOKEN: OnceLock<String> = OnceLock::new();

/// Environment variable the sidecar reads its token from.
pub const TOKEN_ENV: &str = "THETIS_PW_TOKEN";

/// File under the shared data directory holding the running sidecar's token.
const TOKEN_FILE: &str = "browser-token";

/// The shared secret between the tools and the sidecar.
///
/// **Read from a file, not generated per process.** Only the gateway runs
/// [`spawn`], so only the gateway's value is the one the sidecar was actually
/// started with — but `tool_config_json` runs in a *worker*, a separate process
/// with its own `OnceLock` and its own pid. Generating there produced a token
/// the sidecar had never heard of and every `web-browser-*` call came back 403.
///
/// The channel has to be a file rather than an inherited environment variable,
/// because a worker is spawned by whichever gateway is running — normally
/// *trunk's* binary, not the one in a conversation's branch. A branch that adds
/// an `.env()` call to `workers::spawn` therefore changes nothing about how its
/// own workers are launched, and the mismatch survives. The data directory is
/// shared by both processes and is already how they agree on state, so the
/// gateway writes the token there when it starts the sidecar and a worker reads
/// it back. `0600`, and no more secret than the loopback port it guards.
pub fn token(cfg: &Config) -> &'static str {
    TOKEN.get_or_init(|| resolve_token(read_token_file(&cfg.paths.data)))
}

/// Where the token lives, given the shared data directory.
fn token_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(TOKEN_FILE)
}

fn read_token_file(data_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(token_path(data_dir)).ok()
}

/// Records the token for every other process that will need it.
///
/// Called by the gateway just before it starts a sidecar with this value.
fn write_token_file(data_dir: &std::path::Path, token: &str) {
    let path = token_path(data_dir);
    if let Err(e) = std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(&path, token)) {
        // Not fatal: the sidecar still starts, but tools in a worker process
        // will 403 until this succeeds, so it is worth a loud line in the log.
        tracing::warn!(path = %path.display(), error = %e, "could not record the browser token; tools in a worker will not authenticate");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// The token decision, split out so both branches are testable: a `OnceLock`
/// latches on first use, so a test cannot exercise both paths in one process.
fn resolve_token(existing: Option<String>) -> String {
    // A sidecar is already running with this token; anything else is rejected.
    if let Some(found) = existing {
        let found = found.trim();
        if !found.is_empty() {
            return found.to_string();
        }
    }
    // No crypto dependency needed for this: it only has to be unguessable
    // by another process on the same box within one boot.
    let seed = format!(
        "{}-{}-{:p}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        &TOKEN as *const _
    );
    let mut hash: u128 = 0xcbf2_9ce4_8422_2325;
    for b in seed.as_bytes() {
        hash = hash.wrapping_mul(0x100_0000_01b3) ^ u128::from(*b);
    }
    format!("{hash:032x}")
}

/// Whether the sidecar answers a health check right now.
pub async fn healthy(cfg: &BrowserSettings) -> bool {
    let url = format!("{}/health", cfg.base_url());
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Brings the browser stack up, in the background, and keeps it up.
///
/// Deliberately not fatal: a machine with no node, or no network on a cold
/// boot, should still run everything else. The `web-browser-*` tools report the
/// reason when they cannot reach the sidecar, which is a better place for the
/// message than a boot log nobody reads.
pub fn spawn(cfg: Arc<Config>) {
    if !cfg.browser.enabled {
        tracing::info!("the browser sidecar is disabled by configuration");
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = ensure_ready(&cfg.browser).await {
            tracing::warn!(
                error = %format!("{e:#}"),
                "the browser sidecar is not available; the web-browser-* tools will explain why when called"
            );
            return;
        }
        supervise(cfg).await;
    });
}

/// Everything that must be true before the sidecar can start.
pub async fn ensure_ready(cfg: &BrowserSettings) -> Result<()> {
    let dir = &cfg.service_dir;
    if !dir.join("server.js").is_file() {
        anyhow::bail!(
            "no sidecar at {} — expected server.js and package.json there",
            dir.display()
        );
    }

    // Node itself. Without it nothing else in here is worth trying.
    let node = version_of(cfg.node_bin(), &["--version"])
        .await
        .with_context(|| {
            format!(
                "'{}' would not run. Node 18 or newer is what the browser tools need.",
                cfg.node_bin()
            )
        })?;
    tracing::debug!(node = %node.trim(), "found node");

    ensure_playwright(cfg).await?;
    ensure_browser(cfg).await?;
    Ok(())
}

/// Installs the pinned Playwright if the vendored copy is missing or has
/// drifted. A warm tree makes no network call at all.
async fn ensure_playwright(cfg: &BrowserSettings) -> Result<()> {
    let dir = &cfg.service_dir;
    let installed = installed_version(dir).await;
    if installed.as_deref() == Some(cfg.playwright_version.as_str()) {
        tracing::debug!(version = %cfg.playwright_version, "playwright is already installed");
        return Ok(());
    }

    match &installed {
        Some(v) => tracing::info!(
            found = %v,
            want = %cfg.playwright_version,
            "the installed playwright is not the pinned version; reinstalling"
        ),
        None => tracing::info!(
            want = %cfg.playwright_version,
            "playwright is not installed for the browser sidecar; installing"
        ),
    }

    if !cfg.auto_install {
        anyhow::bail!(
            "playwright {} is not installed in {} and browser.auto_install is off. \
             Run `npm install --omit=dev` there to fix it.",
            cfg.playwright_version,
            dir.display()
        );
    }

    // The browsers are downloaded separately, by `ensure_browser`, and only if
    // they are actually absent — so this step must not fetch one.
    let out = Command::new(cfg.npm_bin())
        .args(["install", "--omit=dev", "--no-audit", "--no-fund"])
        .current_dir(dir)
        .env("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD", "1")
        .stdin(Stdio::null())
        .output();

    let out = tokio::time::timeout(cfg.install_timeout, out)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "`npm install` in {} took longer than {}s",
                dir.display(),
                cfg.install_timeout.as_secs()
            )
        })?
        .with_context(|| format!("could not run '{}'", cfg.npm_bin()))?;

    if !out.status.success() {
        anyhow::bail!(
            "`npm install` failed in {}:\n{}",
            dir.display(),
            tail(&String::from_utf8_lossy(&out.stderr), 20)
        );
    }

    let now = installed_version(dir).await;
    if now.as_deref() != Some(cfg.playwright_version.as_str()) {
        anyhow::bail!(
            "after installing, playwright reports {:?} rather than the pinned {}",
            now,
            cfg.playwright_version
        );
    }
    tracing::info!(version = %cfg.playwright_version, "installed playwright for the browser sidecar");
    Ok(())
}

/// Downloads chromium only when it is genuinely missing.
///
/// The pinned Playwright version is chosen to match a browser build that is
/// normally already in the shared cache, so this usually confirms and returns.
async fn ensure_browser(cfg: &BrowserSettings) -> Result<()> {
    if browser_present(cfg).await {
        tracing::debug!("chromium is already installed for playwright");
        return Ok(());
    }
    if !cfg.auto_install {
        anyhow::bail!(
            "chromium is not installed for playwright and browser.auto_install is off. \
             Run `npx playwright install chromium` in {}.",
            cfg.service_dir.display()
        );
    }
    tracing::info!("chromium is missing for playwright; downloading it (this happens once)");

    // `--with-deps` is deliberately not used: it needs root and would try to
    // install system packages under the orchestrator's privileges.
    let out = Command::new(cfg.npm_bin())
        .args([
            "exec",
            "--",
            "playwright",
            "install",
            "chromium",
            "--only-shell",
        ])
        .current_dir(&cfg.service_dir)
        .stdin(Stdio::null())
        .output();

    let out = tokio::time::timeout(cfg.install_timeout, out)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "downloading chromium took longer than {}s",
                cfg.install_timeout.as_secs()
            )
        })?
        .context("could not run the playwright browser installer")?;

    if !out.status.success() {
        anyhow::bail!(
            "installing chromium failed:\n{}",
            tail(&String::from_utf8_lossy(&out.stderr), 20)
        );
    }
    Ok(())
}

/// Asks Playwright itself whether it can find a browser, which is more reliable
/// than guessing at cache paths that differ per platform and per version.
async fn browser_present(cfg: &BrowserSettings) -> bool {
    const PROBE: &str = "try { \
         const p = require('playwright'); \
         const path = p.chromium.executablePath(); \
         process.stdout.write(require('fs').existsSync(path) ? 'yes' : 'no'); \
       } catch (e) { process.stdout.write('no'); }";

    let out = Command::new(cfg.node_bin())
        .args(["-e", PROBE])
        .current_dir(&cfg.service_dir)
        .stdin(Stdio::null())
        .output();

    match tokio::time::timeout(Duration::from_secs(30), out).await {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).trim() == "yes",
        _ => false,
    }
}

/// The Playwright version installed in the sidecar's `node_modules`.
async fn installed_version(dir: &Path) -> Option<String> {
    if !dir.join("node_modules/playwright/package.json").is_file() {
        return None;
    }
    let out = Command::new("node")
        .args([
            "-e",
            "try{process.stdout.write(require('playwright/package.json').version)}catch(e){}",
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output();
    let out = tokio::time::timeout(Duration::from_secs(20), out)
        .await
        .ok()?
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

async fn version_of(bin: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(bin).args(args).stdin(Stdio::null()).output();
    let out = tokio::time::timeout(Duration::from_secs(20), out)
        .await
        .map_err(|_| anyhow::anyhow!("'{bin}' did not answer"))?
        .with_context(|| format!("could not run '{bin}'"))?;
    if !out.status.success() {
        anyhow::bail!("'{bin}' exited {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Runs the sidecar, restarting it if it exits.
///
/// A backoff keeps a sidecar that cannot start (a taken port, a broken install)
/// from becoming a spawn loop in the log.
async fn supervise(cfg: Arc<Config>) {
    let b = &cfg.browser;
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        // Something already listening is the normal case after a worker
        // restart: the gateway owns the sidecar and outlives us.
        if healthy(b).await {
            tokio::time::sleep(Duration::from_secs(15)).await;
            backoff = Duration::from_secs(1);
            continue;
        }

        match start_once(&cfg).await {
            Ok(mut child) => {
                backoff = Duration::from_secs(1);
                let status = child.wait().await;
                tracing::warn!(?status, "the browser sidecar exited; restarting it shortly");
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    backoff_secs = backoff.as_secs(),
                    "could not start the browser sidecar"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Starts one sidecar process and waits for it to answer a health check.
async fn start_once(cfg: &Config) -> Result<tokio::process::Child> {
    let b = &cfg.browser;
    tokio::fs::create_dir_all(&b.artifact_dir).await.ok();

    // Publish before spawning: once the sidecar is up, a tool in any worker
    // process may call it, and this file is how that worker learns the token.
    let tok = token(cfg);
    write_token_file(&cfg.paths.data, tok);

    let mut child = Command::new(b.node_bin())
        .arg("server.js")
        .current_dir(&b.service_dir)
        // The sidecar resolves its artifact directory relative to cwd unless
        // told otherwise, and cwd here is the service directory.
        .env("THETIS_PW_ARTIFACTS", &b.artifact_dir)
        .env("THETIS_PW_PORT", b.port.to_string())
        .env(TOKEN_ENV, tok)
        .env("THETIS_PW_TIMEOUT_MS", b.default_timeout_ms.to_string())
        .env(
            "THETIS_PW_IDLE_MS",
            (b.idle_timeout_secs * 1000).to_string(),
        )
        .env("THETIS_PW_SNAPSHOT_CHARS", b.snapshot_chars.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .with_context(|| format!("spawning the browser sidecar with '{}'", b.node_bin()))?;

    // The sidecar's own logging is useful and would otherwise vanish into a
    // pipe nobody reads — which also eventually blocks the child on a full
    // buffer.
    if let Some(out) = child.stdout.take() {
        pipe_to_log(out, tracing::Level::INFO);
    }
    if let Some(err) = child.stderr.take() {
        pipe_to_log(err, tracing::Level::WARN);
    }

    let deadline = std::time::Instant::now() + b.startup_timeout;
    while std::time::Instant::now() < deadline {
        if healthy(b).await {
            tracing::info!(port = b.port, "the headless browser sidecar is ready");
            return Ok(child);
        }
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!("the sidecar exited during startup ({status})");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = child.start_kill();
    anyhow::bail!(
        "the sidecar did not answer on port {} within {}s",
        b.port,
        b.startup_timeout.as_secs()
    )
}

fn pipe_to_log<R>(reader: R, level: tracing::Level)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match level {
                tracing::Level::WARN => tracing::warn!(target: "browser-sidecar", "{line}"),
                _ => tracing::info!(target: "browser-sidecar", "{line}"),
            }
        }
    });
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_hex_and_long_enough() {
        let generated = resolve_token(None);
        assert_eq!(generated.len(), 32);
        assert!(generated.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// The regression this guards: a worker generating its own token instead of
    /// reading the gateway's made every `web-browser-*` call fail with 403,
    /// because the sidecar only ever accepts the value the gateway started it
    /// with. A recorded token must win over generating a new one.
    #[test]
    fn a_recorded_token_is_used_verbatim() {
        assert_eq!(
            resolve_token(Some("988f72a8056123cb0d624c7eefd08dec".to_string())),
            "988f72a8056123cb0d624c7eefd08dec"
        );
        // A trailing newline from an editor or `echo` must not change the secret.
        assert_eq!(resolve_token(Some("  abc123\n".to_string())), "abc123");
    }

    /// An empty or blank file is the same as no file: generate, rather than
    /// authenticating with the empty string.
    #[test]
    fn a_blank_recorded_token_falls_back_to_generation() {
        assert_eq!(resolve_token(Some(String::new())).len(), 32);
        assert_eq!(resolve_token(Some("   ".to_string())).len(), 32);
    }

    /// The round trip that carries the secret across the process boundary: what
    /// the gateway writes is exactly what a worker reads back.
    #[test]
    fn a_written_token_reads_back_from_the_data_dir() {
        let dir = std::env::temp_dir().join(format!("thetis-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_token_file(&dir), None, "nothing recorded yet");

        write_token_file(&dir, "cafebabe00000000cafebabe00000000");
        assert_eq!(
            resolve_token(read_token_file(&dir)),
            "cafebabe00000000cafebabe00000000"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the token must not be world-readable");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("only", 5), "only");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn base_url_is_loopback() {
        let s = BrowserSettings {
            enabled: true,
            port: 39412,
            service_dir: std::path::PathBuf::from("/tmp/x"),
            node: String::new(),
            npm: "  ".into(),
            playwright_version: "1.61.0".into(),
            auto_install: true,
            install_timeout: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(10),
            default_timeout_ms: 15_000,
            idle_timeout_secs: 900,
            snapshot_chars: 12_000,
            artifact_dir: std::path::PathBuf::from("/tmp/x/art"),
        };
        assert_eq!(s.base_url(), "http://127.0.0.1:39412");
        // Blank means "find it on PATH", for both binaries.
        assert_eq!(s.node_bin(), "node");
        assert_eq!(s.npm_bin(), "npm");
    }
}
