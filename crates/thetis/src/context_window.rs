//! The context window of the model a session is actually using.
//!
//! Compaction used to plan against one number for the whole installation,
//! `context.window_tokens`, set to 200,000. The window is a property of the
//! model, not of the installation: the local llama-server this was found on
//! has an `n_ctx` of 65,536, so the trigger sat three times past the point
//! where the provider refused, and every context failure of the day — a world
//! build refused at 67,960 tokens, a hub turn at 71,048, a fight whose tool
//! call was cut off mid-argument because the model had 490 tokens of room —
//! came from that one gap. None of them looked alike, which is how it lasted.
//!
//! Three sources, in order, and the first that answers wins:
//!
//!   1. The model's own `[[models]]` entry, when it carries a
//!      `context_window`. The operator's word: it may be deliberately smaller
//!      than the model's real window, because a million-token hosted model is
//!      not one anyone wants to fill before summarizing.
//!   2. The server, for a provider with no API key. That is a local
//!      llama-server in practice, and it reports the window it was started
//!      with on `/props` (`default_generation_settings.n_ctx`) and, in newer
//!      builds, on `/v1/models` (`meta.n_ctx`). Read from the server rather
//!      than written down because it changes whenever the server is restarted
//!      with a different `-c`, and nobody updates a config file for that.
//!      Keyed providers are never probed: a key is a secret the guest must not
//!      learn, and a hosted catalogue does not answer `/props` anyway.
//!   3. `context.window_tokens`, for a model nothing is known about.
//!
//! The probe is cached per endpoint, not per call: `Policy::load` runs before
//! every completion, and a request to the model server on each of them would
//! be silly. A success is kept for ten minutes, so a restart with a new size
//! is noticed within that; a failure for thirty seconds, so a server that is
//! down does not add a timeout to every turn while it is.

use crate::config::Config;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a probed window is trusted before the server is asked again.
const FRESH_FOR: Duration = Duration::from_secs(600);
/// How long a failed probe is remembered before it is retried.
const FAILED_FOR: Duration = Duration::from_secs(30);
/// A local server answers `/props` in milliseconds; anything slower is down.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a window came from, for the log line that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Configured,
    Probed,
    Fallback,
}

/// The window compaction should plan against for `model`, and where it came
/// from. Never fails: an unknown model gets the fallback, which is the
/// conservative answer.
pub async fn resolve(cfg: &Config, model: &str) -> (u32, Source) {
    if let Some(window) = cfg.configured_window(model) {
        return (window, Source::Configured);
    }
    let resolved = cfg.resolve_model(model);
    if resolved.provider.api_key.is_none() {
        if let Some(window) = probed(resolved.provider.base_url()).await {
            return (window, Source::Probed);
        }
    }
    (cfg.context.window, Source::Fallback)
}

/// What a probe of one endpoint last said, and when.
struct Probe {
    window: Option<u32>,
    at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Probe>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Probe>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .expect("a plain http client builds")
    })
}

/// The window a keyless server reports for itself, through the cache.
async fn probed(base_url: &str) -> Option<u32> {
    if let Some(hit) = cache().lock().ok()?.get(base_url) {
        let ttl = if hit.window.is_some() {
            FRESH_FOR
        } else {
            FAILED_FOR
        };
        if hit.at.elapsed() < ttl {
            return hit.window;
        }
    }
    let window = probe(base_url).await;
    match window {
        Some(n) => tracing::info!(
            endpoint = base_url,
            n_ctx = n,
            "model server reports its context window"
        ),
        None => tracing::debug!(
            endpoint = base_url,
            "model server did not report a context window"
        ),
    }
    if let Ok(mut cache) = cache().lock() {
        cache.insert(
            base_url.to_string(),
            Probe {
                window,
                at: Instant::now(),
            },
        );
    }
    window
}

/// One round of asking. `/props` hangs off the server root, not the OpenAI
/// prefix, so the `/v1` a provider's base URL carries is stripped for it;
/// `/models` hangs off the prefix as every OpenAI route does.
async fn probe(base_url: &str) -> Option<u32> {
    let base = base_url.trim_end_matches('/');
    let root = base.strip_suffix("/v1").unwrap_or(base);
    if let Some(n) = fetch_json(&format!("{root}/props"))
        .await
        .and_then(|v| window_from_props(&v))
    {
        return Some(n);
    }
    fetch_json(&format!("{base}/models"))
        .await
        .and_then(|v| window_from_models(&v))
}

async fn fetch_json(url: &str) -> Option<serde_json::Value> {
    let response = http().get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// `n_ctx` as llama-server's `/props` reports it. With several slots this is
/// the per-slot size, which is what one request gets.
fn window_from_props(props: &serde_json::Value) -> Option<u32> {
    positive(props.pointer("/default_generation_settings/n_ctx")?)
}

/// `n_ctx` as a newer llama-server's `/v1/models` reports it. Deliberately not
/// `n_ctx_train`, which is what the model was trained to and not what the
/// server was started with — 262,144 against a real 65,536 on the server this
/// was written against.
fn window_from_models(models: &serde_json::Value) -> Option<u32> {
    models
        .get("data")?
        .as_array()?
        .iter()
        .find_map(|m| positive(m.pointer("/meta/n_ctx")?))
}

fn positive(value: &serde_json::Value) -> Option<u32> {
    let n = value.as_u64()?;
    (n > 0 && n <= u64::from(u32::MAX)).then_some(n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shapes two real llama-servers returned on the day, trimmed.
    #[test]
    fn a_servers_own_window_is_read_and_the_training_window_is_not() {
        let props = json!({
            "default_generation_settings": { "n_ctx": 65536, "params": { "seed": 4294967295u32 } },
            "total_slots": 2
        });
        assert_eq!(window_from_props(&props), Some(65_536));
        let models = json!({
            "object": "list",
            "data": [{ "id": "qwen3.8-27b", "meta": { "n_ctx": 65536, "n_ctx_train": 262144 } }]
        });
        assert_eq!(window_from_models(&models), Some(65_536));
        // An older server without `meta.n_ctx` says nothing rather than the
        // training size.
        let older = json!({ "data": [{ "id": "x", "meta": { "n_ctx_train": 262144 } }] });
        assert_eq!(window_from_models(&older), None);
        assert_eq!(window_from_props(&json!({})), None);
        assert_eq!(
            window_from_props(&json!({"default_generation_settings": {"n_ctx": 0}})),
            None
        );
    }

    fn config(extra: &str) -> Config {
        let text = format!(
            r#"
            [llm]
            api_key = "sk-or-test"
            model = "anthropic/claude-sonnet-4.5"

            [[providers]]
            id = "local"
            base_url = "http://127.0.0.1:1/v1"

            [[models]]
            id = "anthropic/claude-sonnet-4.5"
            context_window = 200000

            [[models]]
            id = "local/small"
            provider = "local"
            wire_model = "small"
            {extra}
            "#
        );
        Config::assemble(
            std::path::PathBuf::from("/proj"),
            std::path::PathBuf::from("/proj/thetis.toml"),
            toml::from_str(&text).unwrap(),
            crate::config::Env::None,
        )
        .unwrap()
    }

    /// The effect, per model: the window a policy is handed differs by model,
    /// a configured one wins outright, and a model nothing is known about
    /// gets the conservative fallback — not the largest window in the file.
    #[tokio::test]
    async fn the_window_is_the_models_and_an_unknown_model_gets_the_fallback() {
        let cfg = config("");
        assert_eq!(
            resolve(&cfg, "anthropic/claude-sonnet-4.5").await,
            (200_000, Source::Configured)
        );
        // A keyed provider is never probed, so an unlisted hosted model is
        // simply unknown.
        assert_eq!(
            resolve(&cfg, "anthropic/claude-nobody-knows").await,
            (cfg.context.window, Source::Fallback)
        );
        // A keyless provider is asked; port 1 answers nothing, so this is the
        // fallback too — and it is the same conservative number, not a guess
        // that a local server is large.
        assert_eq!(
            resolve(&cfg, "local/small").await,
            (cfg.context.window, Source::Fallback)
        );
        assert!(cfg.context.window <= 128_000);
    }

    /// A window written on a local model's entry is not second-guessed by a
    /// probe: the operator may have sized it below the server on purpose.
    #[tokio::test]
    async fn a_configured_window_beats_the_probe() {
        let cfg = config("context_window = 8192");
        assert_eq!(
            resolve(&cfg, "local/small").await,
            (8192, Source::Configured)
        );
    }

    /// The real thing, end to end: a stand-in llama-server answers `/props`
    /// with a small `n_ctx`, and that is the window the model resolves to —
    /// smaller than the fallback, which is the whole point. Asked twice, the
    /// server is hit once.
    #[tokio::test]
    async fn a_keyless_server_is_asked_once_and_believed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if request.starts_with("GET /props ") {
                    r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1}"#
                } else {
                    r#"{"error":"not found"}"#
                };
                let status = if request.starts_with("GET /props ") {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let cfg = {
            let text = format!(
                r#"
                [llm]
                api_key = "sk-or-test"
                [[providers]]
                id = "local"
                base_url = "http://{addr}/v1"
                "#
            );
            Config::assemble(
                std::path::PathBuf::from("/proj"),
                std::path::PathBuf::from("/proj/thetis.toml"),
                toml::from_str(&text).unwrap(),
                crate::config::Env::None,
            )
            .unwrap()
        };
        // Unlisted, reached by prefix: exactly the "just loaded a gguf" case.
        assert_eq!(
            resolve(&cfg, "local/whatever").await,
            (4096, Source::Probed)
        );
        assert_eq!(resolve(&cfg, "local/another").await, (4096, Source::Probed));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "one probe per endpoint, not per call"
        );
    }
}
