//! Translation module — orchestrates normalization, glossary, cache, and API client.
//! Phase 2: full selection-mode translation pipeline.

pub mod normalize;
pub mod glossary;
pub mod cache;
pub mod sqlite_cache;
pub mod deepseek;

use std::sync::Arc;

use serde::{Serialize, Deserialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use cache::TranslationCache;
use deepseek::DeepSeekClient;
use glossary::Glossary;
use normalize::PlaceholderProtector;
use sqlite_cache::SqliteCache;

/// Translation request.
#[derive(Debug, Clone, Serialize)]
pub struct TranslateRequest {
    pub request_id: String,
    pub generation: u64,
    pub source_text: String,
    pub source_lang: Option<String>,
    pub target_lang: String,
    pub glossary_entries: Vec<GlossaryEntry>,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlossaryEntry {
    pub source: String,
    pub target: String,
}

/// Translation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslateResult {
    pub request_id: String,
    pub translated_text: String,
    pub source_lang_detected: Option<String>,
    pub tokens_used: u32,
    pub cached: bool,
    pub elapsed_ms: f64,
}

#[derive(Debug)]
pub enum TranslateError {
    ApiError(String),
    RateLimited { retry_after_ms: u64 },
    Timeout,
    Cancelled,
    InvalidInput(String),
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateError::ApiError(msg) => write!(f, "API error: {msg}"),
            TranslateError::RateLimited { retry_after_ms } => {
                write!(f, "rate limited, retry after {retry_after_ms}ms")
            }
            TranslateError::Timeout => write!(f, "translation timeout"),
            TranslateError::Cancelled => write!(f, "translation cancelled"),
            TranslateError::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
        }
    }
}

/// Orchestrates the full translation pipeline:
/// normalize → glossary match → L1 cache → L2 cache → API call → restore placeholders → cache insert.
pub struct TranslationService {
    pub client: DeepSeekClient,
    pub cache: Arc<TranslationCache>,
    pub l2_cache: Option<Arc<SqliteCache>>,
    pub glossary: Arc<Glossary>,
    protector: PlaceholderProtector,
}

impl TranslationService {
    pub fn new(
        client: DeepSeekClient,
        cache: Arc<TranslationCache>,
        l2_cache: Option<Arc<SqliteCache>>,
        glossary: Arc<Glossary>,
    ) -> Self {
        Self {
            client,
            cache,
            l2_cache,
            glossary,
            protector: PlaceholderProtector::new(),
        }
    }

    /// Run the full translation pipeline with optional streaming callback.
    /// `on_chunk` is called with batched delta text (30-50ms intervals).
    pub async fn translate(
        &self,
        request_id: &str,
        source_text: &str,
        target_lang: &str,
        cancel_token: &CancellationToken,
        on_chunk: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<TranslateResult, TranslateError> {
        // 1. Normalize: detect language + protect placeholders
        let norm = normalize::normalize(source_text);
        let src_lang_owned = norm.detected_lang.clone().unwrap_or_else(|| "auto".to_string());
        let src_lang = src_lang_owned.as_str();
        debug!(
            src_lang = src_lang,
            placeholder_count = norm.placeholders.len(),
            "normalized"
        );

        // 2. Glossary: match entries against source text
        let matched_glossary = self.glossary.match_entries(source_text);
        debug!(matched_count = matched_glossary.len(), "glossary_matched");

        // 3. Cache lookup — L1 (memory)
        let cache_key = TranslationCache::compute_key(
            src_lang,
            target_lang,
            self.glossary.version(),
            &norm.normalized_text,
        );
        if let Some(cached) = self.cache.get(&cache_key) {
            info!(request_id = request_id, "L1_cache_hit");
            on_chunk(&cached);
            return Ok(TranslateResult {
                request_id: request_id.to_string(),
                translated_text: cached,
                source_lang_detected: norm.detected_lang,
                tokens_used: 0,
                cached: true,
                elapsed_ms: 0.0,
            });
        }

        // 3b. Cache lookup — L2 (SQLite, TTL 7d)
        if let Some(ref l2) = self.l2_cache {
            if let Some(cached) = l2.get(&cache_key) {
                info!(request_id = request_id, "L2_cache_hit");
                // Promote to L1
                self.cache.insert(cache_key, cached.clone());
                on_chunk(&cached);
                return Ok(TranslateResult {
                    request_id: request_id.to_string(),
                    translated_text: cached,
                    source_lang_detected: norm.detected_lang,
                    tokens_used: 0,
                    cached: true,
                    elapsed_ms: 0.0,
                });
            }
        }

        // 4. API call with streaming
        let mut result = self
            .client
            .translate_stream(
                &norm.normalized_text,
                target_lang,
                &matched_glossary,
                cancel_token,
                on_chunk,
            )
            .await?;

        // 5. Restore placeholders in final text
        result.translated_text = self
            .protector
            .restore(&result.translated_text, &norm.placeholders);
        result.request_id = request_id.to_string();
        result.source_lang_detected = norm.detected_lang;

        // 6. Cache insert — L1 + L2
        self.cache.insert(cache_key, result.translated_text.clone());
        if let Some(ref l2) = self.l2_cache {
            l2.insert(
                &cache_key,
                &result.translated_text,
                src_lang,
                target_lang,
            );
        }

        info!(
            request_id = request_id,
            tokens = result.tokens_used,
            elapsed_ms = result.elapsed_ms,
            "translate_done"
        );

        Ok(result)
    }
}
