//! OCR module — engine interface and Python worker IPC.
//! Phase 2: trait redesign.
//! Phase 3: PythonOcrEngine implementation (PaddleOCR + OpenCV via IPC).
//! Phase 4: RealtimeOcrResult for pixel diff change detection.

pub mod python_engine;

use serde::{Serialize, Deserialize};

pub use python_engine::PythonOcrEngine;

/// OCR request.
#[derive(Debug, Clone, Serialize)]
pub struct OcrRequest {
    pub request_id: String,
    pub generation: u64,
    pub image_data: Vec<u8>, // raw bytes
    pub roi_type: RoiType,
    pub roi_params: RoiParams,
    pub preprocess: PreprocessConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoiType {
    Rect,
    Polygon,
    Perspective,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoiParams {
    Rect { x: u32, y: u32, w: u32, h: u32 },
    Polygon { points: Vec<(u32, u32)> },
    Perspective { corners: [(u32, u32); 4] },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreprocessConfig {
    pub grayscale: bool,
    pub adaptive_threshold: bool,
    pub denoise: bool,
    pub deskew: bool,
}

impl Default for PreprocessConfig {
    fn default() -> Self {
        Self {
            grayscale: true,
            adaptive_threshold: true,
            denoise: true,
            deskew: false,
        }
    }
}

/// OCR result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OcrResult {
    pub request_id: String,
    pub lines: Vec<OcrLine>,
    pub elapsed_ms: f64,
}

/// Phase 4: Realtime OCR result with pixel diff change detection.
#[derive(Debug, Clone)]
pub struct RealtimeOcrResult {
    /// Whether the frame changed compared to the previous one.
    pub changed: bool,
    /// OCR lines (empty if no change).
    pub lines: Vec<OcrLine>,
    /// Mean Absolute Error between current and previous frame.
    pub mae: f64,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OcrLine {
    pub text: String,
    pub confidence: f32,
    pub bbox: (u32, u32, u32, u32), // x, y, w, h
    pub y_center: u32,
}

/// Rust-native OCR engine trait (replaces Python worker architecture).
/// Implementations are synchronous — callers use `spawn_blocking` in the P2 loop.
pub trait OcrEngine: Send + Sync {
    /// Perform OCR on the given request.
    fn recognize(&self, request: OcrRequest) -> Result<OcrResult, OcrError>;

    /// Whether the engine is loaded and ready.
    fn is_available(&self) -> bool;
}

#[derive(Debug)]
pub enum OcrError {
    EngineNotLoaded,
    ProcessingFailed(String),
    Timeout,
    Cancelled,
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrError::EngineNotLoaded => write!(f, "OCR engine not loaded"),
            OcrError::ProcessingFailed(msg) => write!(f, "OCR processing failed: {msg}"),
            OcrError::Timeout => write!(f, "OCR timeout"),
            OcrError::Cancelled => write!(f, "OCR cancelled"),
        }
    }
}

/// Stub OCR engine for Phase 2 (real engine added in Phase 3).
pub struct StubOcrEngine;

impl OcrEngine for StubOcrEngine {
    fn recognize(&self, _request: OcrRequest) -> Result<OcrResult, OcrError> {
        Err(OcrError::EngineNotLoaded)
    }

    fn is_available(&self) -> bool {
        false
    }
}
