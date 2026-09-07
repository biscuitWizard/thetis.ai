//! The campaign knowledge sidecar, and keeping it running.
//!
//! The `rpg-kb-search` and `rpg-kb-ask` tools are `wasm32-wasip2` components:
//! they cannot open SQLite or spawn a process, but every guest is linked
//! against `wasi:http` and loopback is reachable. So the kernel owns one Python
//! process (`services/rpg-kb-sidecar/server.py`) holding the system and
//! campaign stores, and the tools are thin HTTP clients to it — the same shape
//! as the headless browser in [`crate::browser`], and this module is modelled
//! on that one.
//!
//! It owns three things:
//!
//! * **Setup.** Checking the interpreter and the sidecar's files are there.
//!   No install step: the sidecar is standard-library Python.
//! * **Supervision.** Starting the sidecar and restarting it if it dies.
//! * **A token.** Loopback alone would let any local process read a campaign's
//!   private notes and the paid rulebook text, so the kernel generates a token
//!   at boot and tells only the tools and the gateway (through the
//!   `sidecar:rpg-kb` config key).

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::config::{Config, RpgKbSettings};

/// The token this process expects on every sidecar request.
static TOKEN: OnceLock<String> = OnceLock::new();

/// Environment variable the sidecar reads its token from.
pub const TOKEN_ENV: &str = "THETIS_KB_TOKEN";

/// File under the shared data directory holding the running sidecar's token.
/// Same channel as the browser's, for the same reason: a worker is a separate
/// process, spawned by trunk's gateway, and reads the token back from here.
const TOKEN_FILE: &str = "rpg-kb-token";

/// The shared secret between the tools and the sidecar. See
/// [`crate::browser::token`] for why this is a file and not a per-process
/// value.
pub fn token(cfg: &Config) -> &'static str {
    TOKEN.get_or_init(|| crate::browser::resolve_token(read_token_file(&cfg.paths.data)))
}

fn token_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(TOKEN_FILE)
}

fn read_token_file(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(token_path(data_dir)).ok()
}

fn write_token_file(data_dir: &Path, token: &str) {
    let path = token_path(data_dir);
    if let Err(e) = std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(&path, token)) {
        tracing::warn!(path = %path.display(), error = %e, "could not record the rpg-kb token; tools in a worker will not authenticate");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Whether the sidecar answers a health check right now.
pub async fn healthy(cfg: &RpgKbSettings) -> bool {
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

/// Brings the sidecar up, in the background, and keeps it up.
///
/// Deliberately not fatal: a machine without python should still run
/// everything else. The `rpg-kb-*` tools report the reason when they cannot
/// reach the sidecar.
pub fn spawn(cfg: Arc<Config>) {
    if !cfg.rpg_kb.enabled {
        tracing::info!("the rpg-kb sidecar is disabled by configuration");
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = ensure_ready(&cfg.rpg_kb).await {
            tracing::warn!(
                error = %format!("{e:#}"),
                "the rpg-kb sidecar is not available; the rpg-kb-* tools will explain why when called"
            );
            return;
        }
        supervise(cfg).await;
    });
}

/// Everything that must be true before the sidecar can start.
pub async fn ensure_ready(cfg: &RpgKbSettings) -> Result<()> {
    let dir = &cfg.service_dir;
    if !dir.join("server.py").is_file() {
        anyhow::bail!(
            "no sidecar at {} — expected server.py there",
            dir.display()
        );
    }
    let out = Command::new(cfg.python_bin())
        .arg("--version")
        .stdin(Stdio::null())
        .output();
    let out = tokio::time::timeout(Duration::from_secs(20), out)
        .await
        .map_err(|_| anyhow::anyhow!("'{}' did not answer", cfg.python_bin()))?
        .with_context(|| {
            format!(
                "'{}' would not run. Python 3.11 or newer is what the rpg-kb tools need.",
                cfg.python_bin()
            )
        })?;
    if !out.status.success() {
        anyhow::bail!("'{}' exited {}", cfg.python_bin(), out.status);
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !python_is_recent(&version) {
        anyhow::bail!("{version} is too old for the rpg-kb sidecar; it needs Python 3.11 or newer");
    }
    tracing::debug!(python = %version, "found python for the rpg-kb sidecar");
    Ok(())
}

/// `Python 3.11.2` → true; anything below 3.11, or unparseable, → false.
fn python_is_recent(version: &str) -> bool {
    let nums = version.trim().strip_prefix("Python ").unwrap_or(version.trim());
    let mut parts = nums.split('.').map(|p| p.trim().parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => major > 3 || (major == 3 && minor >= 11),
        _ => false,
    }
}

/// Runs the sidecar, restarting it if it exits, with a backoff so a sidecar
/// that cannot start does not become a spawn loop in the log.
async fn supervise(cfg: Arc<Config>) {
    let k = &cfg.rpg_kb;
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        if healthy(k).await {
            tokio::time::sleep(Duration::from_secs(15)).await;
            backoff = Duration::from_secs(1);
            continue;
        }
        match start_once(&cfg).await {
            Ok(mut child) => {
                backoff = Duration::from_secs(1);
                let status = child.wait().await;
                tracing::warn!(?status, "the rpg-kb sidecar exited; restarting it shortly");
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    backoff_secs = backoff.as_secs(),
                    "could not start the rpg-kb sidecar"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Starts one sidecar process and waits for it to answer a health check.
async fn start_once(cfg: &Config) -> Result<tokio::process::Child> {
    let k = &cfg.rpg_kb;
    tokio::fs::create_dir_all(&k.data_dir).await.ok();

    // Publish before spawning, so a tool in any worker can read the token.
    let tok = token(cfg);
    write_token_file(&cfg.paths.data, tok);

    let mut cmd = Command::new(k.python_bin());
    cmd.arg("server.py")
        .current_dir(&k.service_dir)
        .env("THETIS_KB_PORT", k.port.to_string())
        .env(TOKEN_ENV, tok)
        .env("THETIS_KB_DATA_DIR", &k.data_dir)
        .env("THETIS_KB_EMBED_MODEL", &k.embedding_model)
        .env("THETIS_KB_ASK_MODEL", &k.ask_model)
        .env("THETIS_KB_EMBED_URL", &cfg.openrouter_base)
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    // The sidecar embeds and answers through the same provider the kernel
    // uses. Without a key it runs offline: lexical plus a hashed fallback.
    match &cfg.openrouter_api_key {
        Some(key) => {
            cmd.env("OPENROUTER_API_KEY", key.expose());
        }
        None => {
            cmd.env_remove("OPENROUTER_API_KEY");
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning the rpg-kb sidecar with '{}'", k.python_bin()))?;

    if let Some(out) = child.stdout.take() {
        pipe_to_log(out, tracing::Level::INFO);
    }
    if let Some(err) = child.stderr.take() {
        pipe_to_log(err, tracing::Level::WARN);
    }

    let deadline = std::time::Instant::now() + k.startup_timeout;
    while std::time::Instant::now() < deadline {
        if healthy(k).await {
            tracing::info!(port = k.port, "the rpg-kb sidecar is ready");
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
        k.port,
        k.startup_timeout.as_secs()
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
                tracing::Level::WARN => tracing::warn!(target: "rpg-kb-sidecar", "{line}"),
                _ => tracing::info!(target: "rpg-kb-sidecar", "{line}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_version_gate() {
        assert!(python_is_recent("Python 3.11.2"));
        assert!(python_is_recent("Python 3.13.0"));
        assert!(python_is_recent("Python 4.0.0"));
        assert!(!python_is_recent("Python 3.10.12"));
        assert!(!python_is_recent("Python 2.7.18"));
        assert!(!python_is_recent("nonsense"));
    }

    /// The same round trip the browser token makes: what the gateway writes is
    /// what a worker reads back, and it is not world-readable.
    #[test]
    fn a_written_token_reads_back_from_the_data_dir() {
        let dir = std::env::temp_dir().join(format!("thetis-kb-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_token_file(&dir), None);
        write_token_file(&dir, "0123456789abcdef0123456789abcdef");
        assert_eq!(
            crate::browser::resolve_token(read_token_file(&dir)),
            "0123456789abcdef0123456789abcdef"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_path(&dir)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_url_is_loopback_and_python_defaults() {
        let s = RpgKbSettings {
            enabled: false,
            port: 39413,
            data_dir: std::path::PathBuf::from("/tmp/x/data"),
            python_bin: "  ".into(),
            embedding_model: "openai/text-embedding-3-small".into(),
            ask_model: "openai/gpt-4o-mini".into(),
            service_dir: std::path::PathBuf::from("/tmp/x"),
            startup_timeout: Duration::from_secs(30),
        };
        assert_eq!(s.base_url(), "http://127.0.0.1:39413");
        assert_eq!(s.python_bin(), "python3");
    }
}
