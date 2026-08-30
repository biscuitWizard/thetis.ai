//! The chat-completions client.
//!
//! The host owns the socket and the API keys; guests pass request JSON in and
//! pull typed chunks out. Partial tool-call deltas are reassembled here so the
//! agent only ever sees complete tool calls.
//!
//! Any number of OpenAI-compatible endpoints can be configured — OpenRouter, a
//! local llama.cpp server, anything of that shape. The request's `model` field
//! decides which one serves it (see `Config::resolve_model`), and the field is
//! rewritten to that provider's own name for the model before the request goes
//! out. Everything downstream of `send` is provider-agnostic: the wire format
//! is the same, so only the URL, the auth header and the attribution headers
//! differ.

use anyhow::Result;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::bindings::types::{FinishInfo, LlmError, StreamChunk, TokenUsage, ToolCall};
use crate::config::Config;

pub struct LlmClient {
    http: reqwest::Client,
    cfg: Arc<Config>,
    /// The last fully-prepared streaming request body — exactly what went to
    /// the provider, minus the auth header (which is never in the body). The
    /// caller reads it back after `open_stream` and persists it for the web
    /// UI's inspector. One aspect suffices: a worker serves a single session,
    /// and the durable copy is the one in the store.
    last_request: std::sync::Mutex<Option<StoredRequest>>,
    /// Rotates over a provider's replicas. Shared by every request this client
    /// makes, so successive calls land on different endpoints instead of each
    /// starting at the first one.
    next_replica: std::sync::atomic::AtomicUsize,
}

/// A prepared request, as sent, with when it was sent.
#[derive(Clone)]
pub struct StoredRequest {
    pub ts_ms: u64,
    pub body: Arc<serde_json::Value>,
}

/// Receiving end of one in-flight completion.
pub struct StreamHandle {
    rx: mpsc::Receiver<Result<StreamChunk, LlmError>>,
    /// Set once a `finished` chunk has been handed to the guest.
    pub finished: bool,
}

impl StreamHandle {
    pub async fn next(&mut self) -> Result<StreamChunk, LlmError> {
        if self.finished {
            return Err(LlmError::BadRequest(
                "stream already finished; open a new one".into(),
            ));
        }
        match self.rx.recv().await {
            Some(Ok(chunk)) => {
                if matches!(chunk, StreamChunk::Finished(_)) {
                    self.finished = true;
                }
                Ok(chunk)
            }
            Some(Err(e)) => {
                self.finished = true;
                Err(e)
            }
            // Producer dropped without a terminal chunk.
            None => {
                self.finished = true;
                Ok(StreamChunk::Finished(FinishInfo {
                    reason: "eof".into(),
                    usage: None,
                    model: String::new(),
                }))
            }
        }
    }
}

impl LlmClient {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        // `read_timeout`, not `timeout`. reqwest's `timeout` is a *total*
        // deadline: it runs from connect until the body has finished. For a
        // streaming completion the body only finishes when generation does, so
        // a total deadline caps the whole answer — and a slow reasoning model
        // that legitimately streams for longer than the limit has its
        // connection cut mid-body, surfacing as the singularly unhelpful
        // "error decoding response body".
        //
        // A read timeout instead bounds the gap *between* reads and resets on
        // each one. That is the thing actually worth detecting — a server that
        // has stopped talking — and it lets a slow-but-alive stream run as long
        // as it keeps producing. Non-streaming callers restore a total deadline
        // per request, where it is the correct shape.
        let http = reqwest::Client::builder()
            .read_timeout(cfg.request_timeout)
            .build()?;
        Ok(Self {
            http,
            cfg,
            last_request: std::sync::Mutex::new(None),
            next_replica: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// The most recent streaming request body, for the caller to persist.
    pub fn last_request(&self) -> Option<StoredRequest> {
        self.last_request.lock().ok().and_then(|aspect| aspect.clone())
    }

    /// Applies grip defaults to a guest-supplied request body, and works out
    /// which provider is to serve it.
    ///
    /// The returned provider id is resolved from the request's `model` before
    /// the field is rewritten, because the id the picker uses and the name the
    /// endpoint knows the model by need not be the same.
    fn prepare_body(
        &self,
        request_json: &str,
        stream: bool,
    ) -> Result<(serde_json::Value, String), LlmError> {
        let mut body: serde_json::Value = serde_json::from_str(request_json)
            .map_err(|e| LlmError::BadRequest(format!("request is not valid JSON: {e}")))?;

        // Scoped so the borrow ends before caching walks the same value.
        let (model, provider_id) = {
            let obj = body
                .as_object_mut()
                .ok_or_else(|| LlmError::BadRequest("request must be a JSON object".into()))?;

            let requested = obj
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| self.cfg.model.clone());

            let resolved = self.cfg.resolve_model(&requested);
            let provider_id = resolved.provider.id.clone();

            obj.insert("model".into(), resolved.wire_model.clone().into());
            obj.insert("stream".into(), stream.into());
            if stream {
                // Ask for a usage record on the final chunk, which is also the
                // only place cache hits are reported.
                obj.insert(
                    "stream_options".into(),
                    serde_json::json!({ "include_usage": true }),
                );
            }

            // Caching is decided on the *requested* id, not the wire name. The
            // id is the vendor-namespaced one, and the vendor is what says
            // whether breakpoints help — a direct Anthropic-compatible endpoint
            // sending a bare `claude-...` still wants them, and a local server
            // sending a bare model name still does not.
            (requested, provider_id)
        };

        let repaired = normalize_system_roles(&mut body);
        if repaired > 0 {
            tracing::warn!(
                count = repaired,
                "moved stray system messages to the user role; a guest built a request \
                 the provider would have rejected"
            );
        }

        let deduped = dedupe_tool_results(&mut body);
        if deduped > 0 {
            tracing::warn!(
                count = deduped,
                "dropped duplicate tool results; the session log carries more than one \
                 result for the same call (a reconciliation raced a live turn) and the \
                 provider rejects that outright"
            );
        }

        let trimmed = trim_failed_tool_rounds(&mut body);
        if trimmed > 0 {
            tracing::warn!(
                count = trimmed,
                "trimmed failed tool call/result pairs before prompt-cache checkpoints"
            );
        }

        // Last, and only once the model is settled: which provider is about to
        // serve this decides whether breakpoints help or merely cost writes.
        let marked = crate::cache::apply(&mut body, &model, &self.cfg.cache);
        if marked > 0 {
            tracing::debug!(%model, breakpoints = marked, "prompt cache breakpoints applied");
        }

        Ok((body, provider_id))
    }

    async fn send(
        &self,
        body: &serde_json::Value,
        provider_id: &str,
        streaming: bool,
    ) -> Result<reqwest::Response, LlmError> {
        let provider = self
            .cfg
            .provider(provider_id)
            .unwrap_or_else(|| self.cfg.fallback_provider());
        // Claim a slot in the rotation. One increment per request, so
        // concurrent workers spread over the replicas rather than all opening
        // on the first one.
        let slot = self
            .next_replica
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // A local server usually has no key, and an empty bearer token is
        // worse than none: some endpoints reject it outright. OpenRouter, by
        // contrast, cannot serve anything without one, so say so early rather
        // than letting it come back as an opaque 401.
        if provider.api_key.is_none() && provider.is_openrouter() {
            return Err(LlmError::Auth(
                "no API key: set llm.api_key in thetis.toml, or OPENROUTER_API_KEY in the environment"
                    .into(),
            ));
        }

        let mut attempt = 0;
        loop {
            // Each retry advances to the next replica, so a single dead or
            // overloaded endpoint is stepped over rather than hammered. With
            // one endpoint this is the old behaviour exactly.
            let url = provider.url_for("chat/completions", slot + attempt as usize);

            let mut req = self.http.post(&url);
            if !streaming {
                // A non-streaming call has a bounded body, so a total deadline
                // is meaningful and is what the setting has always meant.
                req = req.timeout(self.cfg.request_timeout);
            }
            if let Some(key) = &provider.api_key {
                req = req.bearer_auth(key.expose());
            }
            if provider.is_openrouter() {
                req = req
                    .header("HTTP-Referer", "https://github.com/thetis")
                    .header("X-Title", "Thetis");
            }
            for (name, value) in &provider.headers {
                req = req.header(name.as_str(), value.as_str());
            }

            let result = req.json(body).send().await;

            let retryable = match &result {
                Ok(resp) => {
                    let s = resp.status();
                    s.as_u16() == 429 || s.is_server_error()
                }
                Err(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            };

            if retryable && attempt < self.cfg.max_retries {
                // Exponential backoff with a little jitter from the attempt index.
                let delay = Duration::from_millis(400 * (1 << attempt) + (attempt as u64 * 37));
                tracing::warn!(
                    attempt,
                    ?delay,
                    provider = %provider.id,
                    %url,
                    replicas = provider.replicas(),
                    "llm request failed, retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }

            let resp = result.map_err(|e| LlmError::Transport(e.to_string()))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }

            let detail = resp.text().await.unwrap_or_default();
            let detail = detail.chars().take(600).collect::<String>();
            // Which endpoint refused matters as soon as there is more than
            // one: "404 model not found" reads very differently against a
            // local server than against OpenRouter.
            let detail = format!("[{}] {detail}", provider.id);
            return Err(match status.as_u16() {
                401 | 403 => LlmError::Auth(detail),
                429 => LlmError::RateLimited(detail),
                400 | 404 | 422 => LlmError::BadRequest(detail),
                _ => LlmError::ModelError(format!("http {status}: {detail}")),
            });
        }
    }

    /// Non-streaming completion; returns the raw provider JSON.
    pub async fn chat(&self, request_json: &str) -> Result<String, LlmError> {
        let (body, provider) = self.prepare_body(request_json, false)?;
        let resp = self.send(&body, &provider, false).await?;
        resp.text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))
    }

    /// Opens a streaming completion. Chunks are pumped into the returned handle
    /// by a background task.
    pub async fn open_stream(&self, request_json: &str) -> Result<StreamHandle, LlmError> {
        let (body, provider) = self.prepare_body(request_json, true)?;
        // Streaming requests are the turns themselves (compaction goes through
        // `chat`), so this is the one the inspector wants.
        let body = Arc::new(body);
        if let Ok(mut aspect) = self.last_request.lock() {
            *aspect = Some(StoredRequest {
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                body: body.clone(),
            });
        }
        let resp = self.send(&body, &provider, true).await?;
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            let mut pump = SsePump::new(tx);
            let mut byte_stream = resp.bytes_stream();
            while let Some(next) = byte_stream.next().await {
                match next {
                    Ok(bytes) => {
                        if !pump.feed(&bytes).await {
                            return; // receiver dropped
                        }
                    }
                    Err(e) => {
                        // Mid-body failure. `describe_reqwest` because
                        // reqwest's own Display for a decode error is just
                        // "error decoding response body", which says nothing
                        // about the cause.
                        pump.abort(LlmError::Transport(describe_reqwest(&e))).await;
                        return;
                    }
                }
            }
            pump.finish().await;
        });

        Ok(StreamHandle { rx, finished: false })
    }
}

/// A reqwest error as something a person can act on.
///
/// `reqwest::Error`'s own `Display` is frequently a dead end — a broken stream
/// renders as bare "error decoding response body", with the actual cause (a
/// closed connection, a read timeout, an upstream reset) hidden in the source
/// chain. Walk the chain and append what it says, and classify the shapes worth
/// naming outright.
fn describe_reqwest(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];

    if e.is_timeout() {
        // With `read_timeout` this means the server stopped sending, not that
        // the answer was too long, so say which knob is relevant.
        parts.push(
            "the server stopped sending data (read timeout; see llm.request_timeout_secs)".into(),
        );
    }
    if e.is_body() {
        parts.push("the response body ended early".into());
    }

    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && !parts.iter().any(|p| p == &text) {
            parts.push(text);
        }
        source = std::error::Error::source(cause);
    }

    parts.join(": ")
}

/// The message inside an `LlmError`, for logging.
fn describe(e: &LlmError) -> &str {
    match e {
        LlmError::Auth(m)
        | LlmError::RateLimited(m)
        | LlmError::BadRequest(m)
        | LlmError::ModelError(m)
        | LlmError::Budget(m)
        | LlmError::Transport(m) => m,
    }
}

// --- SSE parsing -----------------------------------------------------------

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// Incrementally turns an SSE byte stream into `StreamChunk`s.
struct SsePump {
    tx: mpsc::Sender<Result<StreamChunk, LlmError>>,
    buf: Vec<u8>,
    tool_calls: BTreeMap<u32, ToolCallAcc>,
    usage: Option<TokenUsage>,
    model: String,
    finish_reason: String,
    done: bool,
    /// Whether any answer text has been handed to the consumer. Reasoning does
    /// not count: it is never part of the persisted message, so a stream that
    /// broke during the thinking phase has produced nothing to salvage.
    streamed_text: bool,
}

impl SsePump {
    fn new(tx: mpsc::Sender<Result<StreamChunk, LlmError>>) -> Self {
        Self {
            tx,
            buf: Vec::new(),
            tool_calls: BTreeMap::new(),
            usage: None,
            model: String::new(),
            finish_reason: String::new(),
            done: false,
            streamed_text: false,
        }
    }

    async fn send(&self, item: Result<StreamChunk, LlmError>) -> bool {
        self.tx.send(item).await.is_ok()
    }

    /// Whether anything usable has arrived: text already streamed to the user,
    /// or tool calls accumulated.
    fn has_partial_work(&self) -> bool {
        self.streamed_text || !self.tool_calls.is_empty()
    }

    /// Ends a stream that broke part-way through.
    ///
    /// A transport failure after some of the answer has arrived is not the same
    /// as a failure to get an answer at all. The user has already watched text
    /// appear, and a bare `Err` throws it away and fails the turn — which is
    /// what "transport error: error decoding response body" looked like from
    /// the outside.
    ///
    /// So when there is partial work, close the stream as `finished` with an
    /// explicit reason instead. The agent keeps what it has, the transcript
    /// stays true, and any tool calls that did complete still run. When there
    /// is nothing to salvage, the error is the whole story and is passed
    /// through unchanged.
    async fn abort(&mut self, err: LlmError) {
        if !self.has_partial_work() {
            let _ = self.send(Err(err)).await;
            return;
        }
        tracing::warn!(
            error = %describe(&err),
            streamed_text = self.streamed_text,
            tool_calls = self.tool_calls.len(),
            "completion stream broke mid-body; keeping what arrived"
        );
        // A truncated tool call is worse than none: half-parsed arguments
        // would be dispatched as if complete. Only keep calls whose arguments
        // are valid JSON on their own.
        self.tool_calls.retain(|_, acc| {
            acc.arguments.trim().is_empty()
                || serde_json::from_str::<serde_json::Value>(&acc.arguments).is_ok()
        });
        self.finish_reason = "error".to_string();
        self.finish().await;
    }

    /// Returns false when the consumer has gone away.
    ///
    /// Bytes are buffered raw and decoded only a complete line at a time. The
    /// network hands us arbitrary chunk boundaries, and a multi-byte character
    /// (an em-dash, a curly quote, an emoji, any CJK text) split across two
    /// chunks would become replacement characters if each chunk were decoded on
    /// its own — or, worse, corrupt the JSON structure of a `data:` line so the
    /// whole delta is dropped. Splitting on the `\n` byte first keeps every
    /// character whole.
    async fn feed(&mut self, bytes: &[u8]) -> bool {
        self.buf.extend_from_slice(bytes);
        while let Some(idx) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=idx).collect();
            let mut end = line_bytes.len() - 1; // drop the '\n'
            if end > 0 && line_bytes[end - 1] == b'\r' {
                end -= 1; // drop a trailing '\r'
            }
            let line = String::from_utf8_lossy(&line_bytes[..end]).into_owned();
            if !self.handle_line(&line).await {
                return false;
            }
        }
        true
    }

    async fn handle_line(&mut self, line: &str) -> bool {
        let Some(payload) = line.strip_prefix("data:") else {
            return true; // comments, empty lines, event: fields
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return true;
        }
        if payload == "[DONE]" {
            self.done = true;
            return true;
        }

        let parsed: SseChunk = match serde_json::from_str(payload) {
            Ok(p) => p,
            // A malformed chunk should not kill an otherwise healthy stream.
            Err(e) => {
                tracing::debug!(error = %e, "skipping unparseable sse chunk");
                return true;
            }
        };

        if let Some(m) = parsed.model {
            self.model = m;
        }
        if let Some(u) = parsed.usage {
            let details = u.prompt_tokens_details.as_ref();
            self.usage = Some(TokenUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                cost_usd: u.cost.unwrap_or(0.0),
                cached_tokens: details.and_then(|d| d.cached_tokens).unwrap_or(0),
                cache_write_tokens: details.and_then(|d| d.cache_write_tokens).unwrap_or(0),
            });
        }

        let Some(choice) = parsed.choices.into_iter().next() else {
            return true;
        };
        if let Some(reason) = choice.finish_reason {
            self.finish_reason = reason;
        }

        if let Some(delta) = choice.delta {
            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                self.streamed_text = true;
                if !self.send(Ok(StreamChunk::Delta(content))).await {
                    return false;
                }
            }
            // Reasoning models send their thinking here. Two spellings are in
            // the wild: `reasoning_content` (DeepSeek, llama.cpp) and
            // `reasoning` (OpenRouter's normalization). Take whichever came.
            if let Some(thought) = delta
                .reasoning_content
                .or(delta.reasoning)
                .filter(|c| !c.is_empty())
            {
                if !self.send(Ok(StreamChunk::Reasoning(thought))).await {
                    return false;
                }
            }
            for tc in delta.tool_calls.unwrap_or_default() {
                let entry = self.tool_calls.entry(tc.index).or_default();
                if let Some(id) = tc.id {
                    entry.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        entry.name.push_str(&name);
                    }
                    if let Some(args) = f.arguments {
                        entry.arguments.push_str(&args);
                    }
                }
            }
        }
        true
    }

    /// Emits the accumulated tool calls and the terminal chunk.
    async fn finish(&mut self) {
        if !self.tool_calls.is_empty() {
            let calls: Vec<ToolCall> = std::mem::take(&mut self.tool_calls)
                .into_values()
                .enumerate()
                .map(|(i, acc)| ToolCall {
                    id: if acc.id.is_empty() {
                        format!("call_{i}")
                    } else {
                        acc.id
                    },
                    name: acc.name,
                    arguments_json: if acc.arguments.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        acc.arguments
                    },
                })
                .collect();
            if !self.send(Ok(StreamChunk::ToolCalls(calls))).await {
                return;
            }
        }

        let reason = if self.finish_reason.is_empty() {
            if self.done {
                "stop".to_string()
            } else {
                "eof".to_string()
            }
        } else {
            std::mem::take(&mut self.finish_reason)
        };

        let usage = self.usage.take();
        let model = std::mem::take(&mut self.model);
        if let Some(u) = &usage {
            // Cache hits are the whole reason the system prompt is assembled
            // in a stable order, so they are worth seeing without a debugger.
            tracing::debug!(
                prompt = u.prompt_tokens,
                completion = u.completion_tokens,
                cached = u.cached_tokens,
                cache_write = u.cache_write_tokens,
                cost = u.cost_usd,
                "token usage"
            );
        }
        self.send(Ok(StreamChunk::Finished(FinishInfo {
            reason,
            usage,
            model,
        })))
        .await;
    }
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: Option<SseDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek and llama.cpp spell it this way.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// OpenRouter normalizes it to this.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<SseToolCall>>,
}

#[derive(Deserialize)]
struct SseToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<SseFunction>,
}

#[derive(Deserialize)]
struct SseFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct SseUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    prompt_tokens_details: Option<SsePromptDetails>,
}

/// Cache accounting, reported the same way by every provider OpenRouter
/// fronts.
#[derive(Deserialize)]
struct SsePromptDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
}

/// Repairs a message array whose `system` messages sit where a provider will
/// refuse them, returning how many were moved.
///
/// Anthropic requires a `system` message to precede an `assistant` message or
/// end the array; one sitting before a `user` or a `tool` result is a hard 400
/// that costs the whole turn. A guest produces that innocently — a note appended
/// mid-conversation lands wherever the log had reached — and since the guest is
/// something the agent can rewrite, the check belongs here as well as there.
///
/// The repair changes the role and nothing else. The text is what carries the
/// meaning, and every provider accepts a user message in any position.
fn normalize_system_roles(body: &mut serde_json::Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };

    let role_at = |messages: &[serde_json::Value], i: usize| -> String {
        messages
            .get(i)
            .and_then(|m| m.get("role"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let mut moved = 0;
    // Index 0 is the system prompt, which is always valid where it is.
    for i in 1..messages.len() {
        if role_at(messages, i) != "system" {
            continue;
        }
        // Allowed: it ends the array, or an assistant turn follows it.
        let last = i + 1 == messages.len();
        if last || role_at(messages, i + 1) == "assistant" {
            continue;
        }
        if let Some(obj) = messages[i].as_object_mut() {
            obj.insert("role".into(), serde_json::Value::from("user"));
            moved += 1;
        }
    }
    moved
}

/// Drops every tool-result message after the first for the same call id,
/// returning how many went.
///
/// Providers hard-400 a request in which one `tool_call` has two results, and
/// the whole turn is lost with it. A log can legitimately end up that way: a
/// reconciliation that raced a live turn once synthesized a failure result
/// for a call whose real result then arrived. The race is fixed at its
/// source, but logs written before the fix — and any corruption not yet
/// imagined — must not brick a conversation forever, so the request is made
/// coherent here regardless. The first result wins: it is the one the model
/// already acted on.
fn dedupe_tool_results(body: &mut serde_json::Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let before = messages.len();
    messages.retain(|message| {
        let is_tool = message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|r| r == "tool");
        if !is_tool {
            return true;
        }
        let Some(id) = message
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)
        else {
            return true;
        };
        seen.insert(id.to_string())
    });
    before - messages.len()
}

/// Removes failed tool calls and their results before cache breakpoints are
/// selected. The guest annotates failed results with `thetis_tool_ok = false`;
/// this host-only marker is consumed here and never reaches a provider.
///
/// A whole failed pair is removed so the remaining request is structurally
/// valid, except that the most recent failed pair is always preserved: it is
/// the feedback the model needs to correct its next action. Successful/unknown
/// results merely lose the private marker. If an assistant mixed failed and
/// successful calls, only the old failed calls go; an assistant message left
/// with neither text nor calls is removed as well.
fn trim_failed_tool_rounds(body: &mut serde_json::Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };

    // Keep the newest failure intact. It is the model's immediate feedback for
    // the next completion; trimming it makes the failed call appear never to
    // have happened and invites the exact same call again. Older failures are
    // low-value history and may be removed before cache anchors are chosen.
    let newest_failed = messages.iter().rev().find_map(|m| {
        (m.get("role").and_then(serde_json::Value::as_str) == Some("tool")
            && m.get("thetis_tool_ok").and_then(serde_json::Value::as_bool) == Some(false))
        .then(|| m.get("tool_call_id").and_then(serde_json::Value::as_str))
        .flatten()
        .map(str::to_string)
    });

    let failed: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| {
            m.get("role").and_then(serde_json::Value::as_str) == Some("tool")
                && m.get("thetis_tool_ok").and_then(serde_json::Value::as_bool) == Some(false)
        })
        .filter_map(|m| m.get("tool_call_id").and_then(serde_json::Value::as_str))
        .filter(|id| newest_failed.as_deref() != Some(*id))
        .map(str::to_string)
        .collect();

    let mut removed = 0;
    messages.retain_mut(|message| {
        let role = message.get("role").and_then(serde_json::Value::as_str).unwrap_or("");
        if role == "tool" {
            let is_failed = message
                .get("tool_call_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| failed.contains(id));
            message.as_object_mut().map(|o| o.remove("thetis_tool_ok"));
            if is_failed {
                removed += 1;
                return false;
            }
        } else if role == "assistant" {
            let empty_text = message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_none_or(str::is_empty);
            if let Some(calls) = message.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                let before = calls.len();
                calls.retain(|call| {
                    call.get("id")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(|id| !failed.contains(id))
                });
                removed += before - calls.len();
                if before > 0 && calls.is_empty() && empty_text {
                    return false;
                }
            }
        }
        true
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client with OpenRouter plus one local llama.cpp-style provider, which
    /// is the arrangement the multi-provider support exists for.
    fn two_provider_client() -> LlmClient {
        let mut cfg = Config::load().expect("the shipped config loads");
        cfg.model = "anthropic/claude-sonnet-4.5".into();
        cfg.providers = vec![
            crate::config::ProviderSpec {
                id: "openrouter".into(),
                label: "OpenRouter".into(),
                base_urls: vec!["https://openrouter.ai/api/v1".into()],
                api_key: Some(crate::config::Secret::new("sk-or-test")),
                headers: Vec::new(),
            },
            crate::config::ProviderSpec {
                id: "local".into(),
                label: "llama.cpp".into(),
                base_urls: vec!["http://127.0.0.1:8080/v1".into()],
                api_key: None,
                headers: Vec::new(),
            },
        ];
        cfg.default_provider = "openrouter".into();
        cfg.models = vec![crate::config::ModelSpec {
            id: "local/qwen3".into(),
            label: "Qwen3 (local)".into(),
            provider: "local".into(),
            wire_model: "qwen3-30b-a3b".into(),
        }];
        LlmClient::new(Arc::new(cfg)).expect("client builds")
    }

    /// A one-shot stand-in for a local llama.cpp server: accepts one request,
    /// hands back the raw bytes it received, and replies with a minimal SSE
    /// stream. Enough to prove which URL was hit, with which body, and with
    /// which headers — the three things provider routing has to get right, and
    /// the three things no unit test on `prepare_body` alone can show.
    async fn stub_server() -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            // Read until the body is in hand. Content-Length is present, so
            // the headers tell us when to stop.
            loop {
                let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf)
                    .await
                    .unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want: usize = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length: ")
                                .or_else(|| l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
            }
            let response = "HTTP/1.1 200 OK\r\n\
                            Content-Type: text/event-stream\r\n\
                            Connection: close\r\n\r\n\
                            data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
            tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                .await
                .unwrap();
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;
            String::from_utf8_lossy(&received).into_owned()
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    #[tokio::test]
    async fn a_local_provider_really_is_called_with_no_auth_header() {
        let (base_url, server) = stub_server().await;

        let mut cfg = Config::load().expect("the shipped config loads");
        cfg.providers = vec![
            crate::config::ProviderSpec {
                id: "openrouter".into(),
                label: "OpenRouter".into(),
                base_urls: vec!["https://openrouter.ai/api/v1".into()],
                api_key: Some(crate::config::Secret::new("sk-or-must-not-be-sent")),
                headers: Vec::new(),
            },
            crate::config::ProviderSpec {
                id: "local".into(),
                label: "llama.cpp".into(),
                base_urls: vec![base_url.clone()],
                api_key: None,
                headers: vec![("X-Test".into(), "yes".into())],
            },
        ];
        cfg.default_provider = "openrouter".into();
        cfg.models = vec![crate::config::ModelSpec {
            id: "local/deepseek".into(),
            label: "DeepSeek (local)".into(),
            provider: "local".into(),
            wire_model: "deepseek-v4-flash".into(),
        }];
        let client = LlmClient::new(Arc::new(cfg)).unwrap();

        let mut stream = client
            .open_stream(r#"{"model":"local/deepseek","messages":[{"role":"user","content":"hi"}]}"#)
            .await
            .expect("the local provider answers");

        // The stream still parses, so nothing about routing disturbed the SSE path.
        let first = stream.next().await.unwrap();
        assert!(matches!(first, StreamChunk::Delta(ref d) if d == "hi"), "{first:?}");

        let request = server.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        let head = head.to_ascii_lowercase();

        // The local provider's own path was hit, not OpenRouter's.
        assert!(head.starts_with("post /v1/chat/completions"), "{head}");
        // No key configured means no header at all — and certainly not the
        // OpenRouter key belonging to a different provider.
        assert!(!head.contains("authorization"), "{head}");
        assert!(!request.contains("sk-or-must-not-be-sent"));
        // OpenRouter's attribution headers are not sent to someone else's server.
        assert!(!head.contains("http-referer"), "{head}");
        assert!(!head.contains("x-title"), "{head}");
        // The provider's own extra headers are.
        assert!(head.contains("x-test: yes"), "{head}");

        // And the endpoint was asked for the name it knows, not the picker's id.
        let sent: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(sent["model"], "deepseek-v4-flash");
        assert_eq!(sent["stream"], true);
    }

    #[test]
    fn replicas_are_handed_out_in_rotation() {
        let provider = crate::config::ProviderSpec {
            id: "local".into(),
            label: "pool".into(),
            base_urls: vec![
                "http://127.0.0.1:8080/v1".into(),
                "http://127.0.0.1:8081/v1".into(),
                "http://127.0.0.1:8082/v1".into(),
            ],
            api_key: None,
            headers: Vec::new(),
        };
        assert_eq!(provider.replicas(), 3);

        // Successive slots walk the pool and wrap.
        let seen: Vec<String> = (0..4)
            .map(|i| provider.url_for("chat/completions", i))
            .collect();
        assert_eq!(seen[0], "http://127.0.0.1:8080/v1/chat/completions");
        assert_eq!(seen[1], "http://127.0.0.1:8081/v1/chat/completions");
        assert_eq!(seen[2], "http://127.0.0.1:8082/v1/chat/completions");
        assert_eq!(seen[3], seen[0], "the rotation wraps");

        // `base_url()` stays the first one, so identity and display are stable
        // no matter which replica a given request used.
        assert_eq!(provider.base_url(), "http://127.0.0.1:8080/v1");
    }

    #[test]
    fn a_single_endpoint_provider_always_uses_it() {
        // The scaling change must not perturb the one-endpoint case: whatever
        // slot the counter has reached, there is only one place to go.
        let provider = crate::config::ProviderSpec {
            id: "openrouter".into(),
            label: "OpenRouter".into(),
            base_urls: vec!["https://openrouter.ai/api/v1".into()],
            api_key: None,
            headers: Vec::new(),
        };
        for i in [0usize, 1, 7, 1000] {
            assert_eq!(
                provider.url_for("chat/completions", i),
                "https://openrouter.ai/api/v1/chat/completions"
            );
        }
    }

    #[tokio::test]
    async fn a_dead_replica_is_stepped_over_on_retry() {
        // Two endpoints, the first of which is not listening. The retry should
        // advance to the second and succeed, rather than retrying the corpse.
        let (live_base, server) = stub_server().await;
        let dead_base = {
            // Bind and drop, so the port is almost certainly unused.
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            drop(l);
            format!("http://127.0.0.1:{port}/v1")
        };

        let mut cfg = Config::load().expect("the shipped config loads");
        cfg.max_retries = 3;
        cfg.providers = vec![crate::config::ProviderSpec {
            id: "local".into(),
            label: "pool".into(),
            base_urls: vec![dead_base, live_base],
            api_key: None,
            headers: Vec::new(),
        }];
        cfg.default_provider = "local".into();
        cfg.models = Vec::new();
        let client = LlmClient::new(Arc::new(cfg)).unwrap();

        let mut stream = client
            .open_stream(r#"{"model":"local/x","messages":[{"role":"user","content":"hi"}]}"#)
            .await
            .expect("the live replica answers after the dead one fails");
        let first = stream.next().await.unwrap();
        assert!(matches!(first, StreamChunk::Delta(ref d) if d == "hi"), "{first:?}");

        // The live server really was the one that served it.
        let request = server.await.unwrap();
        assert!(request.to_ascii_lowercase().starts_with("post /v1/chat/completions"));
    }

    #[test]
    fn a_request_is_routed_to_the_provider_its_model_names() {
        let client = two_provider_client();
        let (body, provider) = client
            .prepare_body(r#"{"model":"local/qwen3","messages":[]}"#, true)
            .unwrap();

        assert_eq!(provider, "local");
        // The picker's id never reaches the endpoint; its own name for the
        // model does.
        assert_eq!(body["model"], "qwen3-30b-a3b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn an_openrouter_model_is_unaffected_by_the_extra_provider() {
        let client = two_provider_client();
        let (body, provider) = client
            .prepare_body(r#"{"model":"anthropic/claude-opus-4.1","messages":[]}"#, false)
            .unwrap();
        assert_eq!(provider, "openrouter");
        assert_eq!(body["model"], "anthropic/claude-opus-4.1");
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn a_request_with_no_model_falls_back_to_the_configured_default() {
        let client = two_provider_client();
        let (body, provider) = client.prepare_body(r#"{"messages":[]}"#, false).unwrap();
        assert_eq!(provider, "openrouter");
        assert_eq!(body["model"], "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn an_unlisted_model_reaches_a_local_provider_by_prefix() {
        // Loading a new gguf should not require editing [[models]] first.
        let client = two_provider_client();
        let (body, provider) = client
            .prepare_body(r#"{"model":"local/mistral-small","messages":[]}"#, false)
            .unwrap();
        assert_eq!(provider, "local");
        assert_eq!(body["model"], "mistral-small");
    }

    fn roles(body: &serde_json::Value) -> Vec<String> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_system_note_before_a_user_message_is_moved() {
        // Exactly the shape that was rejected: a note appended after a tool
        // result, with the conversation carrying on afterwards.
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "user", "content": "go" },
            { "role": "assistant", "content": "working" },
            { "role": "system", "content": "Interrupted: Thetis restarted." },
            { "role": "user", "content": "carry on" },
        ]});

        assert_eq!(normalize_system_roles(&mut body), 1);
        assert_eq!(
            roles(&body),
            ["system", "user", "assistant", "user", "user"]
        );
        // The text is untouched; only where it is allowed to sit changed.
        assert_eq!(body["messages"][3]["content"], "Interrupted: Thetis restarted.");
    }

    #[test]
    fn the_leading_prompt_and_the_allowed_positions_are_left_alone() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "user", "content": "go" },
            // Allowed: an assistant turn follows.
            { "role": "system", "content": "before an assistant" },
            { "role": "assistant", "content": "hi" },
            // Allowed: it ends the array.
            { "role": "system", "content": "at the end" },
        ]});

        assert_eq!(normalize_system_roles(&mut body), 0);
        assert_eq!(roles(&body), ["system", "user", "system", "assistant", "system"]);
    }

    #[test]
    fn a_system_note_before_a_tool_result_is_moved() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "assistant", "content": "", "tool_calls": [] },
            { "role": "system", "content": "note" },
            { "role": "tool", "tool_call_id": "c1", "content": "result" },
        ]});
        assert_eq!(normalize_system_roles(&mut body), 1);
        assert_eq!(roles(&body)[2], "user");
    }

    #[test]
    fn older_failed_tool_pairs_are_trimmed_before_caching() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "bad", "function": { "name": "x", "arguments": "{}" } },
                { "id": "good", "function": { "name": "y", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "bad", "content": "failed", "thetis_tool_ok": false },
            { "role": "tool", "tool_call_id": "good", "content": "worked", "thetis_tool_ok": true },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "latest", "function": { "name": "z", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "latest", "content": "latest failure", "thetis_tool_ok": false }
        ]});

        assert_eq!(trim_failed_tool_rounds(&mut body), 2);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "good");
        assert_eq!(messages[4]["tool_call_id"], "latest");
        assert!(messages.iter().all(|m| m.get("thetis_tool_ok").is_none()));
    }

    #[test]
    fn the_most_recent_failed_tool_pair_is_never_trimmed() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "old", "function": { "name": "git-whoami", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "old", "content": "old failure", "thetis_tool_ok": false },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "new", "function": { "name": "git-whoami", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "new", "content": "fix your credentials", "thetis_tool_ok": false }
        ]});

        assert_eq!(trim_failed_tool_rounds(&mut body), 2);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "new");
        assert_eq!(messages[2]["tool_call_id"], "new");
        assert!(messages[2].get("thetis_tool_ok").is_none());
    }

    #[test]
    fn a_lone_failed_tool_pair_is_preserved() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "only", "function": { "name": "git-whoami", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "only", "content": "fix your credentials", "thetis_tool_ok": false }
        ]});

        assert_eq!(trim_failed_tool_rounds(&mut body), 0);
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert!(body["messages"][2].get("thetis_tool_ok").is_none());
    }

    #[test]
    fn a_request_without_messages_is_not_a_problem() {
        let mut body = serde_json::json!({ "model": "x" });
        assert_eq!(normalize_system_roles(&mut body), 0);
    }

    /// The shape a local DeepSeek-style model actually streams: a long think
    /// in `reasoning_content`, then a short answer in `content`. The two must
    /// come out as different chunk kinds, because only the answer is persisted
    /// and replayed to the model.
    #[tokio::test]
    async fn reasoning_is_kept_apart_from_the_answer() {
        let chunks = drain(concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Okay\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\", so\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"pine\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"apple\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;

        let answer: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Delta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        let thinking: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Reasoning(r) => Some(r.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(answer, "pineapple", "the answer carries no reasoning");
        assert_eq!(thinking, "Okay, so");
    }

    /// OpenRouter normalizes the same field to `reasoning`. Both spellings
    /// must land in the same place, or reasoning silently vanishes depending
    /// on which provider served the request.
    #[tokio::test]
    async fn openrouters_spelling_of_reasoning_is_understood() {
        let chunks = drain(concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"hmm\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::Reasoning(r) if r == "hmm")),
            "{chunks:?}"
        );
    }

    /// A model that sends no reasoning at all must stream exactly as before.
    #[tokio::test]
    async fn a_response_without_reasoning_gains_no_chunks() {
        let chunks = drain(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        assert!(!chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Reasoning(_))));
    }

    /// A slow reasoning model can stream for longer than the timeout while
    /// never actually stalling. `ClientBuilder::timeout` is a deadline on the
    /// *whole* request including the body, so it used to cut such a stream off
    /// mid-answer — surfacing as "error decoding response body". A read timeout
    /// resets on each read, so a trickle keeps the connection alive and only a
    /// genuine stall trips it.
    ///
    /// This drives a real socket that trickles for well over the configured
    /// timeout, so it fails if the timeout shape ever regresses.
    #[tokio::test]
    async fn a_slow_stream_is_not_cut_off_while_it_is_still_sending() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Trickles 8 deltas 60ms apart: 480ms total body, far beyond the
        // 150ms timeout below, but never a 150ms gap.
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = vec![0u8; 8192];
            let _ = sock.read(&mut scratch).await;
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
            for i in 0..8 {
                let body =
                    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{i}\"}}}}]}}\n\n");
                sock.write_all(format!("{:x}\r\n{}\r\n", body.len(), body).as_bytes())
                    .await
                    .unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            }
            sock.write_all(b"0\r\n\r\n").await.unwrap();
            sock.flush().await.unwrap();
        });

        let mut cfg = Config::load().expect("the shipped config loads");
        cfg.request_timeout = Duration::from_millis(150);
        cfg.max_retries = 0;
        cfg.providers = vec![crate::config::ProviderSpec {
            id: "slow".into(),
            label: "Slow".into(),
            base_urls: vec![format!("http://127.0.0.1:{port}/v1")],
            api_key: None,
            headers: Vec::new(),
        }];
        cfg.default_provider = "slow".into();
        cfg.models = Vec::new();
        let client = LlmClient::new(Arc::new(cfg)).unwrap();
        let mut handle = client
            .open_stream(r#"{"model":"slow/trickle","messages":[{"role":"user","content":"hi"}]}"#)
            .await
            .expect("the stream should open");

        let mut text = String::new();
        let mut error = None;
        loop {
            match handle.next().await {
                Ok(StreamChunk::Delta(d)) => text.push_str(&d),
                Ok(StreamChunk::Finished(_)) => break,
                Err(e) => {
                    error = Some(e);
                    break;
                }
                Ok(_) => {}
            }
        }

        assert!(error.is_none(), "a trickling stream was cut off: {error:?}");
        assert_eq!(text, "01234567", "the whole answer should arrive");
    }

    /// The other side of the same coin: a server that goes quiet must still be
    /// given up on, or a wedged endpoint would hang the turn forever.
    #[tokio::test]
    async fn a_stalled_stream_does_eventually_give_up() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Sends one delta, then holds the connection open saying nothing.
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = vec![0u8; 8192];
            let _ = sock.read(&mut scratch).await;
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"stuck\"}}]}\n\n";
            sock.write_all(format!("{:x}\r\n{}\r\n", body.len(), body).as_bytes())
                .await
                .unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });

        let mut cfg = Config::load().expect("the shipped config loads");
        cfg.request_timeout = Duration::from_millis(150);
        cfg.max_retries = 0;
        cfg.providers = vec![crate::config::ProviderSpec {
            id: "stall".into(),
            label: "Stall".into(),
            base_urls: vec![format!("http://127.0.0.1:{port}/v1")],
            api_key: None,
            headers: Vec::new(),
        }];
        cfg.default_provider = "stall".into();
        cfg.models = Vec::new();
        let client = LlmClient::new(Arc::new(cfg)).unwrap();
        let mut handle = client
            .open_stream(r#"{"model":"stall/wedged","messages":[{"role":"user","content":"hi"}]}"#)
            .await
            .expect("the stream should open");

        let mut text = String::new();
        let mut reason = None;
        loop {
            match handle.next().await {
                Ok(StreamChunk::Delta(d)) => text.push_str(&d),
                Ok(StreamChunk::Finished(f)) => {
                    reason = Some(f.reason);
                    break;
                }
                Err(_) => break,
                Ok(_) => {}
            }
        }

        // The delta that did arrive is kept, and the stall is reported as a
        // break rather than a clean stop.
        assert_eq!(text, "stuck");
        assert_eq!(reason.as_deref(), Some("error"));
    }

    /// Feeds a stream that breaks part-way through, and reports what the
    /// consumer saw.
    async fn drain_aborted(sse: &str) -> Vec<Result<StreamChunk, LlmError>> {
        let (tx, mut rx) = mpsc::channel(64);
        let mut pump = SsePump::new(tx);
        for piece in sse.as_bytes().chunks(7) {
            pump.feed(piece).await;
        }
        // Whatever reqwest would have handed us mid-body.
        pump.abort(LlmError::Transport("error decoding response body".into()))
            .await;
        drop(pump);
        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item);
        }
        out
    }

    /// The reported bug: a stream that dies after some of the answer has been
    /// shown must not throw that answer away. The turn should end, not fail.
    #[tokio::test]
    async fn a_broken_stream_keeps_the_text_that_already_arrived() {
        let items = drain_aborted(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"half an \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
        ))
        .await;

        assert!(
            items.iter().all(|i| i.is_ok()),
            "a salvageable stream must not surface an error: {items:?}"
        );
        let text: String = items
            .iter()
            .filter_map(|i| match i {
                Ok(StreamChunk::Delta(d)) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "half an answer");

        // It ends as finished, but honestly: the reason says it broke.
        let reason = items.iter().find_map(|i| match i {
            Ok(StreamChunk::Finished(f)) => Some(f.reason.clone()),
            _ => None,
        });
        assert_eq!(reason.as_deref(), Some("error"));
    }

    /// With nothing to salvage the error is the whole story, and hiding it
    /// behind an empty "finished" would turn a failure into a silent no-op.
    #[tokio::test]
    async fn a_stream_that_breaks_before_anything_arrives_still_errors() {
        let items = drain_aborted("").await;
        assert!(
            matches!(items.first(), Some(Err(LlmError::Transport(_)))),
            "{items:?}"
        );
    }

    /// Reasoning is not the answer: it is never persisted, so a break during
    /// the thinking phase has produced nothing to keep and must still fail.
    #[tokio::test]
    async fn a_break_during_reasoning_is_not_salvageable() {
        let items = drain_aborted(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
        )
        .await;
        assert!(
            items.iter().any(|i| matches!(i, Err(LlmError::Transport(_)))),
            "{items:?}"
        );
    }

    /// A tool call cut off mid-arguments must be dropped, not dispatched: half
    /// a JSON object is not a request anyone should act on.
    #[tokio::test]
    async fn a_truncated_tool_call_is_discarded_rather_than_dispatched() {
        let items = drain_aborted(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"working\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        ))
        .await;

        let calls: Vec<ToolCall> = items
            .iter()
            .filter_map(|i| match i {
                Ok(StreamChunk::ToolCalls(v)) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(calls.is_empty(), "truncated arguments must not survive: {calls:?}");
    }

    /// A complete tool call that happens to be followed by a broken connection
    /// is still good, and re-running the turn without it would lose work.
    #[tokio::test]
    async fn a_complete_tool_call_survives_a_later_break() {
        let items = drain_aborted(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]}}]}\n\n",
        ))
        .await;

        let calls: Vec<ToolCall> = items
            .iter()
            .filter_map(|i| match i {
                Ok(StreamChunk::ToolCalls(v)) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(calls.len(), 1, "{items:?}");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments_json, "{\"path\":\"a\"}");
    }

    async fn drain(sse: &str) -> Vec<StreamChunk> {
        let (tx, mut rx) = mpsc::channel(64);
        let mut pump = SsePump::new(tx);
        // Feed in awkward slices to prove the line buffer handles split frames.
        for piece in sse.as_bytes().chunks(7) {
            pump.feed(piece).await;
        }
        pump.finish().await;
        drop(pump);

        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item.expect("no error expected"));
        }
        out
    }

    #[tokio::test]
    async fn reassembles_content_deltas_across_split_frames() {
        let sse = "data: {\"model\":\"m1\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;

        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Delta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");

        match chunks.last().unwrap() {
            StreamChunk::Finished(f) => {
                assert_eq!(f.reason, "stop");
                assert_eq!(f.model, "m1");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multibyte_characters_survive_a_chunk_boundary() {
        // `drain` feeds the stream in 7-byte slices, so these multi-byte
        // characters (em-dash, curly quotes, an emoji, CJK) are guaranteed to
        // be split across chunk boundaries — exactly the case a per-chunk
        // lossy decode used to turn into replacement characters.
        let content = "café — “π” 😀 日本語";
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\
             data: [DONE]\n",
            serde_json::to_string(content).unwrap()
        );
        let chunks = drain(&sse).await;
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Delta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, content);
        assert!(!text.contains('\u{FFFD}'), "no replacement characters");
    }

    #[tokio::test]
    async fn reassembles_tool_call_argument_fragments() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"exec\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\
                   data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;

        let calls = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::ToolCalls(v) => Some(v.clone()),
                _ => None,
            })
            .expect("tool calls should be emitted");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].arguments_json, r#"{"cmd":"ls"}"#);
        // Arguments must be complete, parseable JSON by the time the agent sees them.
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments_json).unwrap();
        assert_eq!(v["cmd"], "ls");
    }

    #[tokio::test]
    async fn captures_usage_for_spend_accounting() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4,\"cost\":0.002}}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;
        match chunks.last().unwrap() {
            StreamChunk::Finished(f) => {
                let u = f.usage.as_ref().expect("usage captured");
                assert_eq!(u.prompt_tokens, 11);
                assert_eq!(u.completion_tokens, 4);
                assert!((u.cost_usd - 0.002).abs() < 1e-9);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_chunk_does_not_abort_stream() {
        let sse = "data: {not json}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;
        assert!(chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Delta(d) if d == "ok")));
    }

    #[test]
    fn duplicate_tool_results_are_dropped_first_wins() {
        let mut body = serde_json::json!({
            "messages": [
                { "role": "system", "content": "s" },
                { "role": "assistant", "tool_calls": [{ "id": "call_1" }] },
                { "role": "tool", "tool_call_id": "call_1", "content": "real result" },
                { "role": "tool", "tool_call_id": "call_1", "content": "synthetic duplicate" },
                { "role": "tool", "tool_call_id": "call_2", "content": "unrelated" },
                { "role": "user", "content": "next" },
            ]
        });
        assert_eq!(dedupe_tool_results(&mut body), 1);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5);
        let call_1: Vec<_> = messages
            .iter()
            .filter(|m| m["tool_call_id"] == "call_1")
            .collect();
        assert_eq!(call_1.len(), 1);
        assert_eq!(call_1[0]["content"], "real result");
        // Untouched when already coherent.
        assert_eq!(dedupe_tool_results(&mut body), 0);
    }
}
