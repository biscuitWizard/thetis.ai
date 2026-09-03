//! Embedding skill cards, and remembering the result.
//!
//! One provider call shape (`POST {base}/embeddings`) and one cache. Vectors are
//! keyed by `(model, dimensions, content_hash)`, where the hash covers a skill's
//! card text only — see [`crate::skills::Skill::index_text`] — so editing a
//! skill's body does not pay to re-embed it.
//!
//! Every entry point here is fallible in a way the caller can ignore: no API
//! key, a provider outage or a rate limit all return an error that the ranker
//! answers by falling back to BM25. Skills must keep working with the network
//! unplugged, so nothing in this module is allowed to be load-bearing.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::sync::Arc;

use crate::config::Config;
use crate::skills::Skill;

/// How many card texts go in one request. The provider accepts more, but a
/// smaller batch means a transient failure re-does less work.
const BATCH: usize = 64;

/// A vector cache plus the client that fills it.
pub struct Embedder {
    http: reqwest::Client,
    cfg: Arc<Config>,
    persist: crate::persist::Persist,
}

/// What one embedding pass did, for logging and for the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Vectors already in the cache.
    pub hits: usize,
    /// Vectors fetched from the provider.
    pub fetched: usize,
    /// Skills left without a vector because the fetch failed.
    pub missing: usize,
}

impl Embedder {
    pub fn new(cfg: Arc<Config>, persist: crate::persist::Persist) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .build()?;
        Ok(Self { http, cfg, persist })
    }

    fn model(&self) -> &str {
        &self.cfg.skills.embedding_model
    }

    fn dimensions(&self) -> u32 {
        self.cfg.skills.embedding_dimensions
    }

    /// Which endpoint serves embeddings, and under what model name.
    ///
    /// `skills.embedding_provider` names one outright; otherwise the model id
    /// routes the same way a chat model does, so `local/nomic-embed-text` goes
    /// to a llama.cpp server without any further configuration.
    fn endpoint(&self) -> crate::config::ResolvedModel<'_> {
        let configured = self.cfg.skills.embedding_provider.trim();
        if !configured.is_empty() {
            if let Some(provider) = self.cfg.provider(configured) {
                return crate::config::ResolvedModel {
                    wire_model: self.model().to_string(),
                    provider,
                };
            }
            tracing::warn!(
                provider = %configured,
                "skills.embedding_provider is not configured; routing by model id instead"
            );
        }
        self.cfg.resolve_model(self.model())
    }

    /// True when a provider call could succeed. Checked before spending time on
    /// cache lookups that would only be followed by a failed fetch.
    ///
    /// A key is required only where the endpoint requires one: a local server
    /// serving embeddings unauthenticated is perfectly usable.
    pub fn available(&self) -> bool {
        if !self.cfg.skills.retrieval_enabled {
            return false;
        }
        let endpoint = self.endpoint();
        endpoint.provider.api_key.is_some() || !endpoint.provider.is_openrouter()
    }

    /// Returns a vector per skill, in the order given, fetching whatever the
    /// cache is missing.
    ///
    /// A `None` in the result means that skill has no vector and the dense
    /// ranker should ignore it. A total provider failure leaves every entry
    /// `None`, which the caller reads as "use BM25".
    pub async fn vectors_for(&self, skills: &[&Skill]) -> (Vec<Option<Vec<f32>>>, IndexStats) {
        let mut out: Vec<Option<Vec<f32>>> = vec![None; skills.len()];
        let mut stats = IndexStats::default();

        // Cache first, so an unavailable provider still serves whatever was
        // embedded on an earlier run.
        let mut wanted: Vec<usize> = Vec::new();
        for (i, skill) in skills.iter().enumerate() {
            match self.cached(&skill.content_hash).await {
                Some(v) => {
                    out[i] = Some(v);
                    stats.hits += 1;
                }
                None => wanted.push(i),
            }
        }

        if wanted.is_empty() {
            return (out, stats);
        }

        if !self.available() {
            stats.missing = wanted.len();
            tracing::debug!(
                missing = stats.missing,
                "no embedding provider; the ranker will fall back to BM25"
            );
            return (out, stats);
        }

        for chunk in wanted.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|&i| skills[i].index_text()).collect();

            match self.fetch(&texts).await {
                Ok(vectors) if vectors.len() == chunk.len() => {
                    for (&i, vector) in chunk.iter().zip(vectors) {
                        self.store(&skills[i].content_hash, &vector).await;
                        out[i] = Some(vector);
                        stats.fetched += 1;
                    }
                }
                Ok(vectors) => {
                    // A count mismatch means the response cannot be aligned to
                    // the request, so none of it can be trusted.
                    tracing::warn!(
                        asked = chunk.len(),
                        got = vectors.len(),
                        "embedding response length mismatch; discarding the batch"
                    );
                    stats.missing += chunk.len();
                }
                Err(e) => {
                    tracing::warn!(error = %e, batch = chunk.len(), "embedding fetch failed");
                    stats.missing += chunk.len();
                }
            }
        }

        (out, stats)
    }

    /// Embeds one query. Not cached: a query is seen once.
    pub async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        if !self.available() {
            return Err(anyhow!("no embedding provider configured"));
        }
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("cannot embed an empty query"));
        }

        // Long openers cost tokens without helping: the signal for which skill
        // applies is almost always near the top.
        let clipped: String = trimmed
            .chars()
            .take(self.cfg.skills.max_query_chars)
            .collect();

        self.fetch(&[clipped])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("provider returned no embedding for the query"))
    }

    /// One `POST /embeddings`, returning vectors in request order.
    async fn fetch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let endpoint = self.endpoint();
        let provider = endpoint.provider;
        if provider.api_key.is_none() && provider.is_openrouter() {
            return Err(anyhow!("no API key"));
        }

        let url = provider.url("embeddings");
        let body = serde_json::json!({
            "model": endpoint.wire_model,
            "input": texts,
            "dimensions": self.dimensions(),
        });

        let mut request = self.http.post(&url);
        if let Some(key) = &provider.api_key {
            request = request.bearer_auth(key.expose());
        }
        for (name, value) in &provider.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request
            .json(&body)
            .send()
            .await
            .context("embedding request failed")?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            let detail: String = detail.chars().take(400).collect();
            return Err(anyhow!("embedding provider returned {status}: {detail}"));
        }

        let parsed: EmbeddingResponse = response
            .json()
            .await
            .context("embedding response was not the expected JSON")?;

        // The provider is documented to echo request order, but `index` is
        // authoritative and cheap to honour.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);

        let want = self.dimensions() as usize;
        for d in &data {
            if d.embedding.len() != want {
                return Err(anyhow!(
                    "provider returned a {}-d vector, expected {want}; \
                     check that {} honours the `dimensions` parameter",
                    d.embedding.len(),
                    self.model()
                ));
            }
        }

        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    /// Cache key. The model and width are in the key because a vector embedded
    /// by another model, or truncated to another width, is not comparable.
    fn key(&self, content_hash: &str) -> String {
        format!("{}|{}|{}", self.model(), self.dimensions(), content_hash)
    }

    async fn cached(&self, content_hash: &str) -> Option<Vec<f32>> {
        let raw = self
            .persist
            .skill_vector(&self.key(content_hash))
            .await
            .ok()
            .flatten()?;
        decode(&raw, self.dimensions() as usize)
    }

    async fn store(&self, content_hash: &str, vector: &[f32]) {
        if let Err(e) = self
            .persist
            .put_skill_vector(&self.key(content_hash), &encode(vector))
            .await
        {
            // A cache write failure costs money on the next run, not correctness.
            tracing::warn!(error = %e, "could not cache a skill vector");
        }
    }

    /// Drops cached vectors that no live skill refers to.
    pub async fn prune(&self, live: &[&Skill]) -> Result<usize> {
        let keys: Vec<String> = live.iter().map(|s| self.key(&s.content_hash)).collect();
        self.persist.retain_skill_vectors(&keys).await
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// Little-endian f32s. Compact, and decoding is a length check plus a cast.
fn encode(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for f in vector {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Returns `None` when the blob is not `expected_len` floats, which is how a
/// cache written under a different width is rejected rather than misread.
fn decode(raw: &[u8], expected_len: usize) -> Option<Vec<f32>> {
    if raw.len() != expected_len * 4 {
        return None;
    }
    Some(
        raw.chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips() {
        let v = vec![0.0f32, 1.5, -2.25, f32::MIN_POSITIVE];
        let raw = encode(&v);
        assert_eq!(raw.len(), v.len() * 4);
        assert_eq!(decode(&raw, v.len()), Some(v));
    }

    #[test]
    fn decoding_rejects_the_wrong_width() {
        let raw = encode(&[1.0, 2.0, 3.0]);
        assert!(decode(&raw, 3).is_some());
        assert_eq!(decode(&raw, 4), None, "a stale width must not be misread");
        assert_eq!(decode(&raw, 2), None);
    }

    #[test]
    fn decoding_rejects_a_truncated_blob() {
        assert_eq!(decode(&[0, 0, 0], 1), None);
        assert_eq!(decode(&[], 1), None);
        assert_eq!(decode(&[], 0), Some(Vec::new()));
    }

    #[test]
    fn a_response_is_ordered_by_index_not_arrival() {
        let json = r#"{"data":[
            {"index":2,"embedding":[3.0]},
            {"index":0,"embedding":[1.0]},
            {"index":1,"embedding":[2.0]}
        ]}"#;
        let mut parsed: EmbeddingResponse = serde_json::from_str(json).unwrap();
        parsed.data.sort_by_key(|d| d.index);
        let flat: Vec<f32> = parsed.data.iter().map(|d| d.embedding[0]).collect();
        assert_eq!(flat, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_response_without_indices_keeps_its_order() {
        let json = r#"{"data":[
            {"embedding":[1.0]},
            {"embedding":[2.0]}
        ]}"#;
        let parsed: EmbeddingResponse = serde_json::from_str(json).unwrap();
        let flat: Vec<f32> = parsed.data.iter().map(|d| d.embedding[0]).collect();
        assert_eq!(flat, [1.0, 2.0]);
    }
}
