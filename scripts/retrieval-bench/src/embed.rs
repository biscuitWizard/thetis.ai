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
    /// Which endpoint produced these vectors. Absent in files written before
    /// provenance was tracked, which `serde(default)` renders as "" — a value no
    /// live endpoint slug equals, so such a file is discarded rather than
    /// trusted.
    #[serde(default)]
    origin: String,
    vectors: HashMap<String, Vec<f32>>,
}

/// A short filesystem-safe tag for an endpoint, used in the cache filename.
///
/// Only the host matters: two runs against the same host are comparable, and a
/// mock on localhost must never share a file with a real provider.
pub fn endpoint_slug(base: &str) -> String {
    let host = base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("unknown");
    let slug: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_lowercase()
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

        // Which endpoint to ask depends on the key. An OpenRouter key is
        // rejected by OpenAI outright ("Incorrect API key provided"), and
        // OpenRouter *does* serve /embeddings -- verified returning 1536-dim
        // vectors for text-embedding-3-small -- so route by key shape rather
        // than defaulting to OpenAI and failing for the common case here.
        // THETIS_EMBED_BASE overrides both, for a local or proxied endpoint.
        let base = std::env::var("THETIS_EMBED_BASE")
            .ok()
            .filter(|b| !b.trim().is_empty())
            .unwrap_or_else(|| {
                if key.starts_with("sk-or-") {
                    "https://openrouter.ai/api/v1".to_string()
                } else {
                    "https://api.openai.com/v1".to_string()
                }
            });

        // The shipping model id is namespaced ("openai/text-embedding-3-small")
        // because it routes through a gateway. Talking to OpenAI directly, the
        // prefix has to come off or the model is not found.
        let model = model.strip_prefix("openai/").unwrap_or(model).to_string();

        // The cache file is namespaced by endpoint, not just by model, and the
        // endpoint is recorded inside it as well.
        //
        // This is not fussiness. The cache key is (model, dimensions, text hash),
        // which says nothing about *who produced the vector*. A local mock
        // endpoint used to prove the dense path is wired -- returning
        // hash-derived nonsense of the right width -- writes entries that a later
        // real run cannot tell apart from genuine ones, and silently ranks with
        // them. That happened: it made dense retrieval on the skills corpus look
        // catastrophically worse than BM25 (nDCG 0.169 vs 0.460) when real
        // vectors rank the same queries correctly. The scores were coherent
        // enough that nothing looked broken.
        //
        // Two defences, because this failure is silent and expensive:
        // vectors from different endpoints live in different files, and
        // `verify()` checks a cached vector against a freshly fetched one.
        let origin = endpoint_slug(&base);
        let cache_path = cache_dir.join(format!("vectors-{model}-{dimensions}-{origin}.json"));
        let cache = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CacheFile>(&raw).ok())
            .filter(|c| {
                c.model == model && c.dimensions == dimensions
                    // An older cache file predates the origin field; treating a
                    // missing origin as untrusted costs one re-embed and is the
                    // safe direction to fail.
                    && c.origin == origin
            })
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
        // `seen` is what makes this usable on a large corpus: the original
        // dedup was `todo.contains(t)`, a linear scan per text, so 10k documents
        // meant ~50M string comparisons before a single request went out.
        let mut todo: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in texts {
            let k = self.cache_key(t);
            if !self.cache.contains_key(&k) && seen.insert(k) {
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

    /// Confirm a cached vector still matches what the endpoint returns now.
    ///
    /// Cheap insurance (one request) against the failure that produced a
    /// completely wrong published result: a cache holding vectors from a
    /// different producer than the one being reported. Self-similarity for the
    /// same text must be ~1.0; anything less means the cache cannot be trusted
    /// and the dense numbers derived from it are meaningless.
    ///
    /// Returns the *worst* cosine observed, or `None` when nothing among the
    /// samples was cached (a cold cache is not a fault).
    ///
    /// Several texts are checked, spread across the corpus, rather than just the
    /// first: partial poisoning is the likely shape of this fault. A cache can be
    /// filled by more than one run, so the first entry being sound says nothing
    /// about the rest. The worst value is returned because one bad vector is
    /// enough to invalidate a result.
    pub fn verify(&mut self, texts: &[String]) -> Result<Option<f64>> {
        // Spread the probes over the corpus; cap the count so verification stays
        // one cheap request's worth of work regardless of corpus size.
        const PROBES: usize = 8;
        let cached_texts: Vec<String> = texts
            .iter()
            .filter(|t| self.cache.contains_key(&self.cache_key(t)))
            .cloned()
            .collect();
        if cached_texts.is_empty() {
            return Ok(None);
        }
        let stride = (cached_texts.len() / PROBES).max(1);
        let sample: Vec<String> = cached_texts
            .iter()
            .step_by(stride)
            .take(PROBES)
            .cloned()
            .collect();

        let fresh = self.fetch(&sample)?;
        let mut worst = f64::INFINITY;
        for (text, fresh) in sample.iter().zip(fresh.iter()) {
            let cached = match self.cache.get(&self.cache_key(text)) {
                Some(v) => v,
                None => continue,
            };
            let dot: f64 = cached
                .iter()
                .zip(fresh.iter())
                .map(|(a, b)| (*a as f64) * (*b as f64))
                .sum();
            let na: f64 = cached.iter().map(|a| (*a as f64) * (*a as f64)).sum::<f64>().sqrt();
            let nb: f64 = fresh.iter().map(|b| (*b as f64) * (*b as f64)).sum::<f64>().sqrt();
            let sim = if na > 0.0 && nb > 0.0 {
                dot / (na * nb)
            } else {
                0.0
            };
            worst = worst.min(sim);
        }
        Ok(if worst.is_finite() { Some(worst) } else { None })
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
            origin: endpoint_slug(&self.base),
            vectors: self.cache.clone(),
        };
        if let Ok(raw) = serde_json::to_string(&file) {
            let _ = std::fs::write(&self.cache_path, raw);
        }
    }
}
