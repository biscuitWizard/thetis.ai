//! Embeddings for the benchmark, with a disk cache.
//!
//! Deliberately NOT a lift of crates/thetis/src/embeddings.rs, which is the only
//! place in this harness where real source is restated rather than copied. That
//! module reaches for `Config` (the whole 2000-line settings tree) and `Persist`
//! (redb), and pulling either in would drag the orchestrator's dependency graph
//! -- wasmtime included -- into a crate that otherwise compiles in seconds. The
//! trade is accepted because what is restated here is a POST body and a cache
//! key, not ranking logic: if this file drifts from the shipping embedder the
//! benchmark measures slightly different vectors, whereas if `skill_index.rs`
//! drifted the benchmark would measure nothing at all.
//!
//! The cache key matches the shipping one -- (model, dimensions, content_hash)
//! over card text only -- so re-running the benchmark on a revision whose bodies
//! changed but whose cards did not costs no API calls.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const BATCH: usize = 64;

/// Which ranking path a run actually exercised. Recorded in every datapoint,
/// because a dense number and a lexical number are not comparable and a chart
/// that mixed them silently would be worse than no chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Dense,
    Lexical,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Dense => "dense",
            Mode::Lexical => "lexical (bm25 fallback)",
        }
    }
}

pub struct Embedder {
    base: String,
    key: String,
    model: String,
    dimensions: usize,
    cache_path: PathBuf,
    cache: HashMap<String, Vec<f32>>,
    client: reqwest::blocking::Client,
    pub fetched: usize,
    pub hits: usize,
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    model: String,
    dimensions: usize,
    vectors: HashMap<String, Vec<f32>>,
}

/// Hash of the card text, matching the shipping cache key's content component.
pub fn content_hash(card_text: &str) -> String {
    let mut h = Sha256::new();
    h.update(card_text.as_bytes());
    hex::encode(h.finalize())[..32].to_string()
}

impl Embedder {
    /// Builds an embedder if a key is available, else `None` so the caller can
    /// fall back to lexical ranking and label the run.
    pub fn new(cache_dir: &Path, model: &str, dimensions: usize) -> Option<Self> {
        let key = std::env::var("OPENROUTER_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()
            .filter(|k| !k.trim().is_empty())?;

        // OpenRouter does not serve /embeddings, so an OpenRouter-shaped key
        // still has to be pointed at something that does. Overridable for a
        // local or proxied endpoint.
        let base = std::env::var("THETIS_EMBED_BASE")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        // The shipping model id is namespaced ("openai/text-embedding-3-small")
        // because it routes through a gateway. Talking to OpenAI directly, the
        // prefix has to come off or the model is not found.
        let model = model.strip_prefix("openai/").unwrap_or(model).to_string();

        let cache_path = cache_dir.join(format!("vectors-{model}-{dimensions}.json"));
        let cache = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CacheFile>(&raw).ok())
            .filter(|c| c.model == model && c.dimensions == dimensions)
            .map(|c| c.vectors)
            .unwrap_or_default();

        Some(Self {
            base,
            key,
            model,
            dimensions,
            cache_path,
            cache,
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .ok()?,
            fetched: 0,
            hits: 0,
        })
    }

    fn cache_key(&self, text: &str) -> String {
        format!(
            "{}|{}|{}",
            self.model,
            self.dimensions,
            content_hash(text)
        )
    }

    /// Vectors for many texts, in the order given. Cached entries cost nothing.
    pub fn embed_all(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut todo: Vec<String> = Vec::new();
        for t in texts {
            let k = self.cache_key(t);
            if !self.cache.contains_key(&k) && !todo.contains(t) {
                todo.push(t.clone());
            }
        }

        for chunk in todo.chunks(BATCH) {
            let vectors = self.fetch(chunk)?;
            for (text, vector) in chunk.iter().zip(vectors) {
                self.cache.insert(self.cache_key(text), vector);
                self.fetched += 1;
            }
        }

        let out = texts
            .iter()
            .map(|t| {
                let v = self
                    .cache
                    .get(&self.cache_key(t))
                    .cloned()
                    .ok_or_else(|| anyhow!("no vector for text after fetch"))?;
                Ok(v)
            })
            .collect::<Result<Vec<_>>>()?;

        self.hits = texts.len() - self.fetched.min(texts.len());
        self.save();
        Ok(out)
    }

    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_all(&[text.to_string()])?.remove(0))
    }

    fn fetch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        #[derive(Deserialize)]
        struct Resp {
            data: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Item {
            embedding: Vec<f32>,
            index: usize,
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });
        // text-embedding-3-* accept a width; older models reject the field
        // outright, so only send it when it is not the model's native size.
        if self.dimensions != 1536 || self.model.contains("-3-") {
            body["dimensions"] = serde_json::json!(self.dimensions);
        }

        let resp = self
            .client
            .post(format!("{}/embeddings", self.base.trim_end_matches('/')))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .context("embeddings request failed")?;

        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "embeddings endpoint returned {status}: {}",
                text.chars().take(400).collect::<String>()
            ));
        }

        let parsed: Resp = serde_json::from_str(&text)
            .context("embeddings response was not the expected shape")?;

        // The API is documented to preserve input order, but it also returns an
        // index; trusting the index rather than the position costs nothing and
        // would turn a silent mis-pairing of vectors to skills into a non-issue.
        let mut out = vec![Vec::new(); texts.len()];
        for item in parsed.data {
            if item.index < out.len() {
                out[item.index] = item.embedding;
            }
        }
        if out.iter().any(|v| v.is_empty()) {
            return Err(anyhow!("embeddings response was missing a vector"));
        }
        Ok(out)
    }

    fn save(&self) {
        if let Some(parent) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = CacheFile {
            model: self.model.clone(),
            dimensions: self.dimensions,
            vectors: self.cache.clone(),
        };
        if let Ok(raw) = serde_json::to_string(&file) {
            let _ = std::fs::write(&self.cache_path, raw);
        }
    }
}
