//! Translation module (Phase 1 stub with full interface).
//! Real implementation in Phase 2: DeepSeek API via reqwest, caching, retry, etc.

use serde::{Serialize, Deserialize};

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

/// Translator trait (adapter for different backends).
pub trait Translator: Send + Sync {
    fn translate(&self, request: TranslateRequest) -> Result<TranslateResult, TranslateError>;
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

/// Stub translator for Phase 1.
pub struct StubTranslator;

impl Translator for StubTranslator {
    fn translate(&self, req: TranslateRequest) -> Result<TranslateResult, TranslateError> {
        Ok(TranslateResult {
            request_id: req.request_id,
            translated_text: format!("[stub] {}", req.source_text),
            source_lang_detected: Some("en".to_string()),
            tokens_used: 0,
            cached: false,
            elapsed_ms: 0.0,
        })
    }
}
