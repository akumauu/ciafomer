//! OCR coordination module (Phase 1 stub with full interface).
//! Real implementation in Phase 3 will communicate with Python Worker
//! via Named Pipe / Unix Socket + MessagePack.

use serde::{Serialize, Deserialize};

/// OCR request sent to the Python worker.
#[derive(Debug, Clone, Serialize)]
pub struct OcrRequest {
    pub request_id: String,
    pub generation: u64,
    pub image_data: Vec<u8>, // raw bytes, NOT base64
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

/// OCR result from the Python worker.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OcrResult {
    pub request_id: String,
    pub lines: Vec<OcrLine>,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OcrLine {
    pub text: String,
    pub confidence: f32,
    pub bbox: (u32, u32, u32, u32), // x, y, w, h
    pub y_center: u32,
}

/// OCR worker client trait (platform adapter for IPC).
pub trait OcrWorkerClient: Send + Sync {
    fn submit(&self, request: OcrRequest) -> Result<(), OcrError>;
    fn health_check(&self) -> bool;
}

#[derive(Debug)]
pub enum OcrError {
    WorkerNotRunning,
    IpcFailed(String),
    Timeout,
    Cancelled,
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrError::WorkerNotRunning => write!(f, "OCR worker not running"),
            OcrError::IpcFailed(msg) => write!(f, "IPC failed: {msg}"),
            OcrError::Timeout => write!(f, "OCR timeout"),
            OcrError::Cancelled => write!(f, "OCR cancelled"),
        }
    }
}

/// Stub OCR client for Phase 1.
pub struct StubOcrClient;

impl OcrWorkerClient for StubOcrClient {
    fn submit(&self, _request: OcrRequest) -> Result<(), OcrError> {
        Err(OcrError::WorkerNotRunning)
    }

    fn health_check(&self) -> bool {
        false
    }
}
