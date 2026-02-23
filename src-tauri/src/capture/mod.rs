//! Text capture module — clipboard-based capture for Linux/WSL.
//! Uses xdotool to simulate Ctrl+C and xclip to read/restore clipboard.
//! Startup capability detection: probes for xdotool/xclip availability once.

use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

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
    ToolNotAvailable(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NoSelection => write!(f, "no text selected"),
            CaptureError::AccessibilityTimeout => write!(f, "accessibility API timeout"),
            CaptureError::ClipboardFailed(msg) => write!(f, "clipboard failed: {msg}"),
            CaptureError::PlatformUnsupported => write!(f, "platform unsupported"),
            CaptureError::ToolNotAvailable(tool) => write!(f, "required tool not found: {tool}"),
        }
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
        captured_at_us: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64,
    }
}

/// Clipboard-based text capture for Linux/WSL.
/// Requires `xdotool` and `xclip` on PATH.
pub struct ClipboardCapture {
    /// Wait after simulating Ctrl+C before reading clipboard.
    copy_wait: Duration,
    /// Maximum total time for the capture operation.
    _total_timeout: Duration,
    /// Cached probe result: true if both xdotool and xclip are available.
    tools_available: bool,
}

impl ClipboardCapture {
    /// Create a new ClipboardCapture, probing for tool availability at construction time.
    pub fn new(copy_wait: Duration, total_timeout: Duration) -> Self {
        let xdotool_ok = probe_command("xdotool");
        let xclip_ok = probe_command("xclip");

        if !xdotool_ok {
            warn!("xdotool not found — text capture will be unavailable");
        }
        if !xclip_ok {
            warn!("xclip not found — text capture will be unavailable");
        }

        let tools_available = xdotool_ok && xclip_ok;
        if tools_available {
            info!("clipboard capture: xdotool + xclip available");
        }

        Self {
            copy_wait,
            _total_timeout: total_timeout,
            tools_available,
        }
    }
}

impl TextCapture for ClipboardCapture {
    fn capture_selection(&self) -> Result<String, CaptureError> {
        // Fast fail if tools were not found at startup
        if !self.tools_available {
            return Err(CaptureError::ToolNotAvailable(
                "xdotool and/or xclip".into(),
            ));
        }

        // Backup current clipboard and install restore guard
        let backup = read_clipboard().ok();
        let _guard = ClipboardGuard {
            backup: backup.clone(),
        };

        // Simulate Ctrl+C
        let copy_result = Command::new("xdotool")
            .args(["key", "--clearmodifiers", "ctrl+c"])
            .output();

        match copy_result {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return Err(CaptureError::ClipboardFailed(format!(
                    "xdotool failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Err(e) => {
                return Err(CaptureError::ClipboardFailed(format!(
                    "xdotool exec failed: {e}"
                )));
            }
        }

        // Wait for clipboard to update
        std::thread::sleep(self.copy_wait);

        // Read new clipboard content
        let new_content = read_clipboard().map_err(|e| {
            CaptureError::ClipboardFailed(format!("read after copy failed: {e}"))
        })?;

        // Check if content actually changed (not the same as backup)
        if let Some(ref old) = backup {
            if new_content == *old {
                return Err(CaptureError::NoSelection);
            }
        }

        if new_content.trim().is_empty() {
            return Err(CaptureError::NoSelection);
        }

        debug!(len = new_content.len(), "clipboard_captured");
        Ok(new_content)
    }
}

/// RAII guard that restores clipboard content on drop (finally guarantee).
struct ClipboardGuard {
    backup: Option<String>,
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if let Some(ref content) = self.backup {
            // Best-effort restore; ignore errors
            if let Err(e) = write_clipboard(content) {
                debug!(error = %e, "clipboard restore failed (best-effort)");
            }
        }
    }
}

/// Read current clipboard content via xclip.
fn read_clipboard() -> Result<String, String> {
    let output = Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .output()
        .map_err(|e| format!("xclip exec: {e}"))?;

    if !output.status.success() {
        // Empty clipboard returns non-zero; treat as empty
        return Ok(String::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Write content to clipboard via xclip.
fn write_clipboard(content: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard", "-i"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("xclip spawn: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| format!("xclip write: {e}"))?;
    }

    child.wait().map_err(|e| format!("xclip wait: {e}"))?;
    Ok(())
}

/// Probe whether a command is available on PATH.
fn probe_command(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
