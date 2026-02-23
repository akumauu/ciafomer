//! Text capture module (Phase 1 stub with full interface).
//! Implements: accessibility API → clipboard fallback → TextPacket output.
//! Platform-specific capture behind trait adapter.

use serde::Serialize;

/// Unified text packet — downstream doesn't care about source.
#[derive(Debug, Clone, Serialize)]
pub struct TextPacket {
    pub text: String,
    pub source: CaptureSource,
    pub request_id: String,
    pub generation: u64,
    pub captured_at_us: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum CaptureSource {
    Accessibility,
    Clipboard,
    Ocr,
}

/// Platform-agnostic capture trait.
pub trait TextCapture: Send + Sync {
    /// Attempt to capture selected text. Returns the captured text or error.
    fn capture_selection(&self) -> Result<String, CaptureError>;
}

#[derive(Debug)]
pub enum CaptureError {
    NoSelection,
    AccessibilityTimeout,
    ClipboardFailed(String),
    PlatformUnsupported,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NoSelection => write!(f, "no text selected"),
            CaptureError::AccessibilityTimeout => write!(f, "accessibility API timeout"),
            CaptureError::ClipboardFailed(msg) => write!(f, "clipboard failed: {msg}"),
            CaptureError::PlatformUnsupported => write!(f, "platform unsupported"),
        }
    }
}

/// Stub capture implementation for Phase 1.
/// Will be replaced with real accessibility API + clipboard fallback in Phase 2.
pub struct StubCapture;

impl TextCapture for StubCapture {
    fn capture_selection(&self) -> Result<String, CaptureError> {
        Err(CaptureError::PlatformUnsupported)
    }
}

/// Create a TextPacket from captured text.
pub fn make_text_packet(
    text: String,
    source: CaptureSource,
    request_id: String,
    generation: u64,
) -> TextPacket {
    TextPacket {
        text,
        source,
        request_id,
        generation,
        captured_at_us: 0,
    }
}
