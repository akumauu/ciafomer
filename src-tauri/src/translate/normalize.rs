//! Language detection and placeholder protection.
//! Normalizes source text before translation to preserve untranslatable tokens
//! and detect source language for cache keying.

use regex::Regex;

/// Result of normalizing source text.
#[derive(Debug, Clone)]
pub struct NormalizeResult {
    pub normalized_text: String,
    pub detected_lang: Option<String>,
    pub placeholders: Vec<PlaceholderEntry>,
}

/// A placeholder substitution that must be restored after translation.
#[derive(Debug, Clone)]
pub struct PlaceholderEntry {
    pub tag: String,      // e.g. "<<PH0>>"
    pub original: String, // e.g. "https://example.com"
}

/// Detects the dominant language of `text` using whatlang.
/// Returns an ISO 639-1 code or None if detection is unreliable.
pub fn detect_language(text: &str) -> Option<String> {
    let info = whatlang::detect(text)?;
    if !info.is_reliable() {
        return None;
    }
    Some(lang_to_code(info.lang()))
}

fn lang_to_code(lang: whatlang::Lang) -> String {
    use whatlang::Lang::*;
    match lang {
        Eng => "en",
        Cmn => "zh",
        Jpn => "ja",
        Kor => "ko",
        Fra => "fr",
        Deu => "de",
        Spa => "es",
        Rus => "ru",
        Por => "pt",
        Ita => "it",
        Ara => "ar",
        Hin => "hi",
        Tur => "tr",
        Vie => "vi",
        Tha => "th",
        Nld => "nl",
        Pol => "pl",
        Ukr => "uk",
        _ => "other",
    }
    .to_string()
}

/// Protects untranslatable tokens (URLs, emails, numbers with units, inline code)
/// by replacing them with placeholder tags before translation.
pub struct PlaceholderProtector {
    patterns: Vec<Regex>,
}

impl PlaceholderProtector {
    pub fn new() -> Self {
        Self {
            patterns: vec![
                // URLs
                Regex::new(r"https?://[^\s,，。)）\]]+").unwrap(),
                // Emails
                Regex::new(r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}").unwrap(),
                // Numbers with units (e.g. 3.14kg, 100%, 42px, $99.99)
                Regex::new(r"[$€¥£]?\d+(?:\.\d+)?(?:%|px|em|rem|pt|kg|km|mb|gb|tb|ms|fps|hz)\b")
                    .unwrap(),
                // Standalone numbers / decimals / negative numbers
                Regex::new(r"\b\d+(?:\.\d+)?\b").unwrap(),
                // Inline code (backtick-wrapped)
                Regex::new(r"`[^`]+`").unwrap(),
            ],
        }
    }

    /// Replace matched tokens with `<<PH0>>`, `<<PH1>>`, etc.
    /// Returns the protected text and the list of entries for later restoration.
    pub fn protect(&self, text: &str) -> (String, Vec<PlaceholderEntry>) {
        let mut entries = Vec::new();
        let mut result = text.to_string();

        for pat in &self.patterns {
            // Collect matches first to avoid borrow issues during replacement
            let matches: Vec<String> = pat
                .find_iter(&result)
                .map(|m| m.as_str().to_string())
                .collect();

            for m in matches {
                let idx = entries.len();
                let tag = format!("<<PH{}>>", idx);
                // Replace only the first occurrence of this exact match
                result = result.replacen(&m, &tag, 1);
                entries.push(PlaceholderEntry {
                    tag,
                    original: m,
                });
            }
        }

        (result, entries)
    }

    /// Restore placeholders in the translated text.
    pub fn restore(&self, text: &str, entries: &[PlaceholderEntry]) -> String {
        let mut result = text.to_string();
        for entry in entries {
            result = result.replace(&entry.tag, &entry.original);
        }
        result
    }
}

/// Run the full normalization pipeline on source text.
pub fn normalize(text: &str) -> NormalizeResult {
    let detected_lang = detect_language(text);
    let protector = PlaceholderProtector::new();
    let (normalized_text, placeholders) = protector.protect(text);
    NormalizeResult {
        normalized_text,
        detected_lang,
        placeholders,
    }
}
