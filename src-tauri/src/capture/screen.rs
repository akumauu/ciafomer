//! Screen capture module for Linux/WSL.
//! Captures the full screen or a region using available system tools
//! (scrot, maim, grim). Returns raw PNG bytes.

use std::process::Command;
use tracing::{debug, info, warn};

/// Screen capture backend.
#[derive(Debug, Clone, Copy)]
pub enum CaptureBackend {
    Scrot,
    Maim,
    Grim,
}

/// Captured screenshot data.
pub struct Screenshot {
    pub png_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum ScreenCaptureError {
    NoBackendAvailable,
    CaptureFailed(String),
    IoError(String),
}

impl std::fmt::Display for ScreenCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenCaptureError::NoBackendAvailable => {
                write!(f, "no screen capture tool available (need scrot, maim, or grim)")
            }
            ScreenCaptureError::CaptureFailed(msg) => write!(f, "capture failed: {msg}"),
            ScreenCaptureError::IoError(msg) => write!(f, "IO error: {msg}"),
        }
    }
}

/// Detect available screen capture backend.
pub fn detect_backend() -> Option<CaptureBackend> {
    // Prefer grim (Wayland/WSLg), then maim (X11), then scrot (X11)
    for (name, backend) in [
        ("grim", CaptureBackend::Grim),
        ("maim", CaptureBackend::Maim),
        ("scrot", CaptureBackend::Scrot),
    ] {
        if probe_command(name) {
            info!(backend = name, "screen capture backend detected");
            return Some(backend);
        }
    }
    warn!("no screen capture backend found");
    None
}

/// Capture the full screen and return PNG bytes.
pub fn capture_full_screen(backend: CaptureBackend) -> Result<Vec<u8>, ScreenCaptureError> {
    let tmp_path = "/tmp/ciallo_capture.png";

    let result = match backend {
        CaptureBackend::Scrot => Command::new("scrot")
            .args(["-z", "-o", tmp_path])
            .output(),
        CaptureBackend::Maim => Command::new("maim")
            .arg(tmp_path)
            .output(),
        CaptureBackend::Grim => Command::new("grim")
            .arg(tmp_path)
            .output(),
    };

    let output = result.map_err(|e| {
        ScreenCaptureError::CaptureFailed(format!("failed to run capture tool: {e}"))
    })?;

    if !output.status.success() {
        return Err(ScreenCaptureError::CaptureFailed(format!(
            "capture tool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let bytes = std::fs::read(tmp_path).map_err(|e| {
        ScreenCaptureError::IoError(format!("failed to read screenshot: {e}"))
    })?;

    // Clean up temp file (best-effort)
    let _ = std::fs::remove_file(tmp_path);

    debug!(size = bytes.len(), "screen captured");
    Ok(bytes)
}

/// Screen capture manager with cached backend detection.
pub struct ScreenCapture {
    backend: Option<CaptureBackend>,
}

impl ScreenCapture {
    /// Create a new ScreenCapture, probing for available backends.
    pub fn new() -> Self {
        Self {
            backend: detect_backend(),
        }
    }

    /// Whether screen capture is available.
    pub fn is_available(&self) -> bool {
        self.backend.is_some()
    }

    /// Capture the full screen as PNG bytes.
    pub fn capture(&self) -> Result<Vec<u8>, ScreenCaptureError> {
        let backend = self
            .backend
            .ok_or(ScreenCaptureError::NoBackendAvailable)?;
        capture_full_screen(backend)
    }
}

fn probe_command(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
