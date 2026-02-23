//! Glossary loading and matching.
//! Loads term pairs from JSON, matches source entries against input text,
//! and returns only the matched entries for prompt injection.

use serde::Deserialize;
use std::path::Path;

use super::GlossaryEntry;

/// On-disk glossary file format.
#[derive(Debug, Deserialize)]
struct GlossaryFile {
    version: u32,
    entries: Vec<GlossaryEntry>,
}

/// Loaded glossary with version tracking for cache key computation.
pub struct Glossary {
    version: u32,
    entries: Vec<GlossaryEntry>,
}

#[derive(Debug)]
pub enum GlossaryError {
    Io(std::io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for GlossaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GlossaryError::Io(e) => write!(f, "glossary IO error: {e}"),
            GlossaryError::Parse(e) => write!(f, "glossary parse error: {e}"),
        }
    }
}

impl From<std::io::Error> for GlossaryError {
    fn from(e: std::io::Error) -> Self {
        GlossaryError::Io(e)
    }
}

impl From<serde_json::Error> for GlossaryError {
    fn from(e: serde_json::Error) -> Self {
        GlossaryError::Parse(e)
    }
}

impl Glossary {
    /// Load glossary from a JSON file.
    pub fn load_from_file(path: &Path) -> Result<Self, GlossaryError> {
        let content = std::fs::read_to_string(path)?;
        let file: GlossaryFile = serde_json::from_str(&content)?;
        Ok(Self {
            version: file.version,
            entries: file.entries,
        })
    }

    /// Create an empty glossary (fallback when file is missing).
    pub fn empty() -> Self {
        Self {
            version: 0,
            entries: Vec::new(),
        }
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// Return entries whose `source` term appears in `text` (case-insensitive).
    pub fn match_entries(&self, text: &str) -> Vec<GlossaryEntry> {
        let text_lower = text.to_lowercase();
        self.entries
            .iter()
            .filter(|e| text_lower.contains(&e.source.to_lowercase()))
            .cloned()
            .collect()
    }
}
