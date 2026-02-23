//! Python OCR Worker IPC client.
//! Spawns python-worker/worker.py, communicates via stdin/stdout with
//! MessagePack framing (4-byte BE length prefix + msgpack payload).
//! Health check: ping every 30s, 500ms pong timeout, 3 consecutive failures → restart.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::{OcrEngine, OcrError, OcrLine, OcrRequest, OcrResult, RoiParams, RoiType};

/// Managed Python worker process with stdin/stdout handles.
struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl WorkerProcess {
    /// Send a msgpack message with 4-byte BE length prefix.
    fn send(&mut self, msg: &[u8]) -> Result<(), OcrError> {
        let len = msg.len() as u32;
        self.stdin
            .write_all(&len.to_be_bytes())
            .map_err(|e| OcrError::ProcessingFailed(format!("write len: {e}")))?;
        self.stdin
            .write_all(msg)
            .map_err(|e| OcrError::ProcessingFailed(format!("write payload: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| OcrError::ProcessingFailed(format!("flush: {e}")))?;
        Ok(())
    }

    /// Read a msgpack response with 4-byte BE length prefix.
    fn recv(&mut self) -> Result<Vec<u8>, OcrError> {
        let mut len_buf = [0u8; 4];
        self.stdout
            .read_exact(&mut len_buf)
            .map_err(|e| OcrError::ProcessingFailed(format!("read len: {e}")))?;
        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len > 50 * 1024 * 1024 {
            return Err(OcrError::ProcessingFailed(format!(
                "message too large: {msg_len}"
            )));
        }
        let mut payload = vec![0u8; msg_len];
        self.stdout
            .read_exact(&mut payload)
            .map_err(|e| OcrError::ProcessingFailed(format!("read payload: {e}")))?;
        Ok(payload)
    }

    /// Check if the child process is still alive.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        // Best-effort shutdown
        let shutdown_msg = WorkerMessage::Shutdown;
        if let Ok(bytes) = rmp_serde::to_vec(&shutdown_msg) {
            let _ = self.send(&bytes);
        }
        // Give it a moment then kill
        std::thread::sleep(Duration::from_millis(100));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- IPC message types ---

#[derive(Serialize)]
#[serde(tag = "type")]
enum WorkerMessage {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "ocr")]
    Ocr {
        request_id: String,
        #[serde(with = "serde_bytes")]
        image_data: Vec<u8>,
        roi_type: String,
        roi_params: serde_json::Value,
        preprocess: PreprocessMsg,
    },
    #[serde(rename = "shutdown")]
    Shutdown,
}

mod serde_bytes {
    use serde::Serializer;
    pub fn serialize<S: Serializer>(data: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(data)
    }
}

#[derive(Serialize)]
struct PreprocessMsg {
    grayscale: bool,
    adaptive_threshold: bool,
    denoise: bool,
    deskew: bool,
}

#[derive(Deserialize, Debug)]
struct WorkerResponse {
    #[serde(rename = "type")]
    msg_type: String,
    #[allow(dead_code)]
    request_id: Option<String>,
    lines: Option<Vec<WorkerOcrLine>>,
    elapsed_ms: Option<f64>,
    message: Option<String>,
}

#[derive(Deserialize, Debug)]
struct WorkerOcrLine {
    text: String,
    confidence: f64,
    bbox: (u32, u32, u32, u32),
    y_center: u32,
}

/// Python OCR Worker engine. Manages the worker process lifecycle.
pub struct PythonOcrEngine {
    worker: Mutex<Option<WorkerProcess>>,
    python_bin: String,
    worker_script: PathBuf,
    available: AtomicBool,
    consecutive_health_failures: AtomicU32,
    max_health_failures: u32,
}

impl PythonOcrEngine {
    /// Create a new PythonOcrEngine.
    /// `python_bin`: path to Python interpreter (e.g., "python3" or venv path).
    /// `worker_script`: path to python-worker/worker.py.
    pub fn new(python_bin: &str, worker_script: PathBuf) -> Self {
        Self {
            worker: Mutex::new(None),
            python_bin: python_bin.to_string(),
            worker_script,
            available: AtomicBool::new(false),
            consecutive_health_failures: AtomicU32::new(0),
            max_health_failures: 3,
        }
    }

    /// Spawn (or respawn) the Python worker process.
    fn spawn_worker(&self) -> Result<WorkerProcess, OcrError> {
        info!(
            script = %self.worker_script.display(),
            python = %self.python_bin,
            "spawning Python OCR worker"
        );

        let mut child = Command::new(&self.python_bin)
            .arg(&self.worker_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // let worker stderr pass through for logging
            .spawn()
            .map_err(|e| {
                OcrError::ProcessingFailed(format!("failed to spawn Python worker: {e}"))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            OcrError::ProcessingFailed("failed to get worker stdin".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            OcrError::ProcessingFailed("failed to get worker stdout".into())
        })?;

        info!("Python OCR worker spawned (pid: {:?})", child.id());
        self.available.store(true, Ordering::SeqCst);
        self.consecutive_health_failures.store(0, Ordering::SeqCst);

        Ok(WorkerProcess {
            child,
            stdin,
            stdout,
        })
    }

    /// Ensure a worker is running and return a mutable reference.
    fn ensure_worker(&self) -> Result<(), OcrError> {
        let mut guard = self.worker.lock();
        let needs_spawn = match guard.as_mut() {
            Some(w) => !w.is_alive(),
            None => true,
        };
        if needs_spawn {
            info!("worker not running, spawning...");
            let new_worker = self.spawn_worker()?;
            *guard = Some(new_worker);
        }
        Ok(())
    }

    /// Send an IPC message and receive the response.
    fn send_recv(&self, msg: &WorkerMessage) -> Result<WorkerResponse, OcrError> {
        self.ensure_worker()?;
        let mut guard = self.worker.lock();
        let worker = guard
            .as_mut()
            .ok_or(OcrError::EngineNotLoaded)?;

        let payload =
            rmp_serde::to_vec(msg).map_err(|e| OcrError::ProcessingFailed(format!("serialize: {e}")))?;
        worker.send(&payload)?;
        let response_bytes = worker.recv()?;
        let response: WorkerResponse = rmp_serde::from_slice(&response_bytes)
            .map_err(|e| OcrError::ProcessingFailed(format!("deserialize: {e}")))?;
        Ok(response)
    }

    /// Perform a health check (ping/pong).
    pub fn health_check(&self) -> bool {
        match self.send_recv(&WorkerMessage::Ping) {
            Ok(resp) if resp.msg_type == "pong" => {
                self.consecutive_health_failures.store(0, Ordering::SeqCst);
                debug!("health check: pong received");
                true
            }
            Ok(resp) => {
                warn!(msg_type = %resp.msg_type, "unexpected health check response");
                self.record_health_failure();
                false
            }
            Err(e) => {
                warn!(error = %e, "health check failed");
                self.record_health_failure();
                false
            }
        }
    }

    fn record_health_failure(&self) {
        let failures = self.consecutive_health_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if failures >= self.max_health_failures {
            warn!(
                failures,
                "max health failures reached, marking unavailable and killing worker"
            );
            self.available.store(false, Ordering::SeqCst);
            // Kill the current worker so next request spawns a fresh one
            let mut guard = self.worker.lock();
            *guard = None;
        }
    }

    /// Start a background health check loop (call from a spawned thread).
    pub fn start_health_loop(engine: std::sync::Arc<Self>, interval: Duration) {
        std::thread::Builder::new()
            .name("ocr-health-check".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(interval);
                    if engine.available.load(Ordering::SeqCst)
                        || engine.worker.lock().is_some()
                    {
                        engine.health_check();
                    }
                }
            })
            .expect("failed to spawn health check thread");
    }

    /// Convert OcrRequest's ROI to worker message format.
    fn roi_to_msg(roi_type: &RoiType, roi_params: &RoiParams) -> (String, serde_json::Value) {
        match (roi_type, roi_params) {
            (RoiType::Rect, RoiParams::Rect { x, y, w, h }) => (
                "rect".to_string(),
                serde_json::json!({"x": x, "y": y, "w": w, "h": h}),
            ),
            (RoiType::Polygon, RoiParams::Polygon { points }) => (
                "polygon".to_string(),
                serde_json::json!({"points": points}),
            ),
            (RoiType::Perspective, RoiParams::Perspective { corners }) => (
                "perspective".to_string(),
                serde_json::json!({
                    "corners": corners.iter()
                        .map(|(x, y)| vec![*x, *y])
                        .collect::<Vec<_>>()
                }),
            ),
            // Fallback: if types mismatch, send as fullframe
            _ => (
                "fullframe".to_string(),
                serde_json::json!({}),
            ),
        }
    }
}

impl OcrEngine for PythonOcrEngine {
    fn recognize(&self, request: OcrRequest) -> Result<OcrResult, OcrError> {
        let (roi_type_str, roi_params_val) =
            Self::roi_to_msg(&request.roi_type, &request.roi_params);

        let msg = WorkerMessage::Ocr {
            request_id: request.request_id.clone(),
            image_data: request.image_data,
            roi_type: roi_type_str,
            roi_params: roi_params_val,
            preprocess: PreprocessMsg {
                grayscale: request.preprocess.grayscale,
                adaptive_threshold: request.preprocess.adaptive_threshold,
                denoise: request.preprocess.denoise,
                deskew: request.preprocess.deskew,
            },
        };

        let start = Instant::now();
        let response = self.send_recv(&msg)?;

        match response.msg_type.as_str() {
            "ocr_result" => {
                let lines = response
                    .lines
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| OcrLine {
                        text: l.text,
                        confidence: l.confidence as f32,
                        bbox: l.bbox,
                        y_center: l.y_center,
                    })
                    .collect();

                let elapsed_ms = response
                    .elapsed_ms
                    .unwrap_or_else(|| start.elapsed().as_secs_f64() * 1000.0);

                Ok(OcrResult {
                    request_id: request.request_id,
                    lines,
                    elapsed_ms,
                })
            }
            "error" => {
                let msg = response.message.unwrap_or_else(|| "unknown error".into());
                Err(OcrError::ProcessingFailed(msg))
            }
            other => Err(OcrError::ProcessingFailed(format!(
                "unexpected response type: {other}"
            ))),
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
}
