//! DeepSeek API translation client.
//! Connection pooling via reqwest, manual SSE parsing, simple token-bucket
//! rate limiting, retry logic per spec.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{GlossaryEntry, TranslateError, TranslateResult};

/// DeepSeek chat/completions streaming client.
pub struct DeepSeekClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Simple token-bucket: tracks the next allowed request time.
    next_allowed: Arc<tokio::sync::Mutex<Instant>>,
    /// Minimum interval between requests (e.g. 100ms = 10 req/s).
    min_interval: Duration,
}

impl DeepSeekClient {
    /// Create a new client. Reads `DEEPSEEK_API_KEY` from environment.
    pub fn new() -> Result<Self, TranslateError> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").map_err(|_| {
            TranslateError::InvalidInput("DEEPSEEK_API_KEY environment variable not set".into())
        })?;

        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| TranslateError::ApiError(e.to_string()))?;

        Ok(Self {
            http,
            api_key,
            base_url: "https://api.deepseek.com".into(),
            next_allowed: Arc::new(tokio::sync::Mutex::new(Instant::now())),
            min_interval: Duration::from_millis(100), // 10 req/s
        })
    }

    /// Wait until the rate limiter allows a request.
    async fn rate_limit_wait(&self) {
        let mut next = self.next_allowed.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep(*next - now).await;
        }
        *next = Instant::now() + self.min_interval;
    }

    /// Translate with SSE streaming. Calls `on_chunk` for each content delta.
    /// The callback receives accumulated text per flush batch (30-50ms batching).
    pub async fn translate_stream(
        &self,
        source_text: &str,
        target_lang: &str,
        glossary: &[GlossaryEntry],
        cancel_token: &CancellationToken,
        on_chunk: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<TranslateResult, TranslateError> {
        // Rate limit
        self.rate_limit_wait().await;

        if cancel_token.is_cancelled() {
            return Err(TranslateError::Cancelled);
        }

        let max_tokens = estimate_max_tokens(source_text);
        let user_prompt = build_user_prompt(source_text, target_lang, glossary);

        let body = serde_json::json!({
            "model": "deepseek-chat",
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": user_prompt}
            ],
            "max_tokens": max_tokens,
            "stream": true,
            "temperature": 0.1
        });

        let start = Instant::now();
        let response = self.send_with_retry(&body, cancel_token).await?;

        // Parse SSE stream with batched flushing
        let mut full_text = String::new();
        let mut batch_buf = String::new();
        let mut last_flush = Instant::now();
        let mut tokens_used: u32 = 0;
        let flush_interval = Duration::from_millis(40);

        let mut stream = response.bytes_stream();

        // SSE line buffer for partial lines across chunks
        let mut line_buf = String::new();

        while let Some(chunk_result) = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel_token.cancelled() => {
                return Err(TranslateError::Cancelled);
            }
        } {
            let bytes = chunk_result.map_err(|e| TranslateError::ApiError(e.to_string()))?;
            let text = String::from_utf8_lossy(&bytes);
            line_buf.push_str(&text);

            // Process complete lines
            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.starts_with("data: ") {
                    let data = &line[6..];
                    if data == "[DONE]" {
                        // Flush remaining buffer
                        if !batch_buf.is_empty() {
                            on_chunk(&batch_buf);
                            batch_buf.clear();
                        }
                        continue;
                    }

                    if let Ok(parsed) = serde_json::from_str::<SseChunk>(data) {
                        if let Some(choice) = parsed.choices.first() {
                            if let Some(ref content) = choice.delta.content {
                                full_text.push_str(content);
                                batch_buf.push_str(content);
                            }
                        }
                        if let Some(usage) = parsed.usage {
                            tokens_used = usage.total_tokens;
                        }
                    }
                }
            }

            // Batch flush: emit accumulated content every ~40ms
            if !batch_buf.is_empty() && last_flush.elapsed() >= flush_interval {
                on_chunk(&batch_buf);
                batch_buf.clear();
                last_flush = Instant::now();
            }
        }

        // Final flush
        if !batch_buf.is_empty() {
            on_chunk(&batch_buf);
        }

        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        Ok(TranslateResult {
            request_id: String::new(), // filled by caller
            translated_text: full_text,
            source_lang_detected: None,
            tokens_used,
            cached: false,
            elapsed_ms,
        })
    }

    /// Send request with retry logic.
    /// 429: Retry-After or 1s/2s/4s (max 3).
    /// 5xx: exponential backoff (max 2).
    /// Timeout: immediate retry once.
    async fn send_with_retry(
        &self,
        body: &serde_json::Value,
        cancel_token: &CancellationToken,
    ) -> Result<reqwest::Response, TranslateError> {
        let mut attempt: u32 = 0;
        let max_429_retries: u32 = 3;
        let max_5xx_retries: u32 = 2;
        let mut timeout_retried = false;

        loop {
            let result = self
                .http
                .post(format!("{}/v1/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(body)
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    return Ok(resp);
                }
                Ok(resp) if resp.status().as_u16() == 429 => {
                    if attempt >= max_429_retries {
                        return Err(TranslateError::RateLimited { retry_after_ms: 0 });
                    }
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| Duration::from_secs(1 << attempt));
                    warn!(attempt, wait_ms = wait.as_millis() as u64, "429 rate limited, retrying");
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancel_token.cancelled() => return Err(TranslateError::Cancelled),
                    }
                    attempt += 1;
                }
                Ok(resp) if resp.status().is_server_error() => {
                    if attempt >= max_5xx_retries {
                        return Err(TranslateError::ApiError(format!(
                            "server error: {}",
                            resp.status()
                        )));
                    }
                    let wait = Duration::from_millis(500 * (1 << attempt));
                    warn!(
                        attempt,
                        status = resp.status().as_u16(),
                        wait_ms = wait.as_millis() as u64,
                        "5xx error, retrying"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancel_token.cancelled() => return Err(TranslateError::Cancelled),
                    }
                    attempt += 1;
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body_text = resp.text().await.unwrap_or_default();
                    return Err(TranslateError::ApiError(format!(
                        "unexpected status {}: {}",
                        status,
                        body_text.chars().take(200).collect::<String>()
                    )));
                }
                Err(e) if e.is_timeout() => {
                    if timeout_retried {
                        return Err(TranslateError::Timeout);
                    }
                    warn!("request timeout, retrying once");
                    timeout_retried = true;
                }
                Err(e) => {
                    return Err(TranslateError::ApiError(e.to_string()));
                }
            }
        }
    }
}

// --- Prompt construction ---

/// System prompt kept under 60 tokens.
const SYSTEM_PROMPT: &str = "You are a translator. Output only the translation, nothing else.";

/// Build compact user prompt: {"t":"text","l":"lang"} or {"t":"text","l":"lang","g":{"src":"tgt",...}}
fn build_user_prompt(text: &str, target_lang: &str, glossary: &[GlossaryEntry]) -> String {
    let escaped = escape_json_string(text);
    let lang = escape_json_string(target_lang);
    if glossary.is_empty() {
        format!("{{\"t\":\"{}\",\"l\":\"{}\"}}", escaped, lang)
    } else {
        let g: Vec<String> = glossary
            .iter()
            .map(|e| {
                format!(
                    "\"{}\":\"{}\"",
                    escape_json_string(&e.source),
                    escape_json_string(&e.target)
                )
            })
            .collect();
        format!("{{\"t\":\"{}\",\"l\":\"{}\",\"g\":{{{}}}}}", escaped, lang, g.join(","))
    }
}

/// Estimate max_tokens: (input_tokens * 1.15 + 32), capped at 768.
fn estimate_max_tokens(text: &str) -> u32 {
    // Rough: ~4 chars/token for Latin, ~1.5 for CJK
    let estimated_input_tokens = text.len() as f64 / 3.0;
    let max = (estimated_input_tokens * 1.15 + 32.0) as u32;
    max.min(768).max(64)
}

/// Escape a string for embedding inside a JSON string value.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// --- SSE response types ---

#[derive(Deserialize)]
struct SseChunk {
    choices: Vec<SseChoice>,
    usage: Option<SseUsage>,
}

#[derive(Deserialize)]
struct SseChoice {
    delta: SseDelta,
}

#[derive(Deserialize)]
struct SseDelta {
    content: Option<String>,
}

#[derive(Deserialize)]
struct SseUsage {
    total_tokens: u32,
}
