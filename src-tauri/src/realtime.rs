//! Phase 4: Realtime incremental translation.
//! 500ms periodic sampling → pixel diff (MAE) → OCR if changed →
//! line-hash diff (text + y_bucket 8px) → translate only added lines →
//! merge with cached translations → render.
//!
//! Key design:
//! - Pixel diff happens in Python worker (MAE on ROI-cropped frames)
//! - Line diff uses blake3(text | y_bucket) to identify unchanged lines
//! - Per-line translation cache avoids re-translating unchanged text
//! - Token saving tracked: lines_from_cache / total_lines >= 40%

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::Manager;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn, error};

use crate::capture::screen::ScreenCapture;
use crate::history::{HistoryStore, HistoryRecord};
use crate::metrics::{MetricsRegistry, metric_names};
use crate::ocr::{OcrLine, PythonOcrEngine, PreprocessConfig};
use crate::state_machine::StateMachine;
use crate::translate::TranslationService;

/// Default sampling interval for realtime mode.
const SAMPLE_INTERVAL_MS: u64 = 500;
/// y_bucket granularity for line position bucketing.
const Y_BUCKET_PX: u32 = 8;

/// Computes a blake3 hash for a line using text + y_bucket.
/// Two lines with the same text at approximately the same vertical position
/// (within Y_BUCKET_PX) produce the same hash.
fn line_hash(text: &str, y_center: u32) -> [u8; 32] {
    let y_bucket = (y_center / Y_BUCKET_PX) * Y_BUCKET_PX;
    let mut hasher = blake3::Hasher::new();
    hasher.update(text.as_bytes());
    hasher.update(b"|");
    hasher.update(&y_bucket.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Result of line-level diff between two OCR frames.
struct LineDiff {
    /// Lines present in new but not in old (need translation).
    added: Vec<OcrLine>,
    /// Lines present in both old and new (reuse cached translation).
    unchanged: Vec<OcrLine>,
}

/// Compute line-level diff between previous and current OCR lines.
fn diff_lines(old_lines: &[OcrLine], new_lines: &[OcrLine]) -> LineDiff {
    let old_hashes: HashSet<[u8; 32]> = old_lines
        .iter()
        .map(|l| line_hash(&l.text, l.y_center))
        .collect();

    let mut added = Vec::new();
    let mut unchanged = Vec::new();

    for line in new_lines {
        let hash = line_hash(&line.text, line.y_center);
        if old_hashes.contains(&hash) {
            unchanged.push(line.clone());
        } else {
            added.push(line.clone());
        }
    }

    LineDiff { added, unchanged }
}

/// Per-session state for realtime incremental translation.
struct RealtimeSession {
    /// Previous OCR lines (for diff computation).
    previous_lines: Vec<OcrLine>,
    /// Line text → translated text cache (session-local fast path).
    line_cache: HashMap<String, String>,
    /// Stats for token saving calculation.
    total_lines_seen: u64,
    lines_translated_via_api: u64,
    lines_from_cache: u64,
    /// Number of frames where no change was detected (skipped OCR).
    frames_no_change: u64,
    /// Number of frames where change was detected (ran OCR).
    frames_changed: u64,
}

impl RealtimeSession {
    fn new() -> Self {
        Self {
            previous_lines: Vec::new(),
            line_cache: HashMap::new(),
            total_lines_seen: 0,
            lines_translated_via_api: 0,
            lines_from_cache: 0,
            frames_no_change: 0,
            frames_changed: 0,
        }
    }

    /// Build merged translation from current lines + line cache.
    /// Returns (source_text, translated_text) for rendering.
    fn build_merged(&self, current_lines: &[OcrLine]) -> (String, String) {
        let mut source_parts = Vec::with_capacity(current_lines.len());
        let mut translated_parts = Vec::with_capacity(current_lines.len());

        for line in current_lines {
            source_parts.push(line.text.as_str());
            if let Some(translated) = self.line_cache.get(&line.text) {
                translated_parts.push(translated.as_str());
            } else {
                // Fallback: if somehow not cached, show original
                translated_parts.push(line.text.as_str());
            }
        }

        (source_parts.join("\n"), translated_parts.join("\n"))
    }

    /// Token saving percentage.
    fn token_saving_pct(&self) -> f64 {
        let total = self.lines_from_cache + self.lines_translated_via_api;
        if total == 0 {
            return 0.0;
        }
        (self.lines_from_cache as f64 / total as f64) * 100.0
    }

    /// Summary stats as JSON-serializable map.
    fn stats_json(&self) -> serde_json::Value {
        serde_json::json!({
            "total_lines_seen": self.total_lines_seen,
            "lines_translated_via_api": self.lines_translated_via_api,
            "lines_from_cache": self.lines_from_cache,
            "token_saving_pct": self.token_saving_pct(),
            "frames_no_change": self.frames_no_change,
            "frames_changed": self.frames_changed,
        })
    }
}

/// Run the realtime incremental translation loop.
/// Captures the screen every 500ms, sends to Python worker for pixel diff + OCR,
/// performs line-level diff, translates only new lines, and renders merged result.
///
/// This function runs as a tokio task and respects the provided CancellationToken.
pub async fn run_realtime_loop(
    ocr_engine: Arc<PythonOcrEngine>,
    screen_capture: Arc<ScreenCapture>,
    translation_service: Arc<TranslationService>,
    cancel_token: CancellationToken,
    roi_type: String,
    roi_params: serde_json::Value,
    app_handle: tauri::AppHandle,
    metrics: Arc<MetricsRegistry>,
    state_machine: Arc<StateMachine>,
    history_store: Option<Arc<HistoryStore>>,
) {
    use tauri::Emitter;

    let interval = Duration::from_millis(SAMPLE_INTERVAL_MS);
    let preprocess = PreprocessConfig::default();
    let mut session = RealtimeSession::new();

    // Reset realtime state in Python worker (clear previous frame)
    {
        let engine = Arc::clone(&ocr_engine);
        let _ = tokio::task::spawn_blocking(move || engine.reset_realtime()).await;
    }

    let _ = app_handle.emit("realtime-started", serde_json::json!({}));
    info!("realtime loop started (roi_type={}, interval={}ms)", roi_type, SAMPLE_INTERVAL_MS);

    loop {
        let cycle_start = Instant::now();

        if cancel_token.is_cancelled() {
            break;
        }

        // 1. Capture screen
        let capture_result = {
            let sc = Arc::clone(&screen_capture);
            tokio::task::spawn_blocking(move || sc.capture()).await
        };

        if cancel_token.is_cancelled() {
            break;
        }

        let screenshot = match capture_result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => {
                warn!(error = %e, "realtime screen capture failed");
                let _ = app_handle.emit("realtime-error", serde_json::json!({
                    "error": format!("Screen capture failed: {e}"),
                }));
                // Wait and retry
                tokio::time::sleep(interval).await;
                continue;
            }
            Err(e) => {
                error!(error = %e, "realtime capture task panicked");
                break;
            }
        };

        // 2. Send to OCR worker for realtime_ocr (pixel diff + OCR)
        let ocr_result = {
            let engine = Arc::clone(&ocr_engine);
            let roi_t = roi_type.clone();
            let roi_p = roi_params.clone();
            let pp = preprocess.clone();
            tokio::task::spawn_blocking(move || {
                engine.realtime_ocr(screenshot, &roi_t, &roi_p, &pp)
            })
            .await
        };

        if cancel_token.is_cancelled() {
            break;
        }

        match ocr_result {
            Ok(Ok(result)) if !result.changed => {
                // No change detected — skip OCR and translation
                session.frames_no_change += 1;
                debug!(
                    mae = result.mae,
                    elapsed_ms = result.elapsed_ms,
                    "realtime: no change"
                );

                let elapsed = cycle_start.elapsed();
                if elapsed < interval {
                    tokio::time::sleep(interval - elapsed).await;
                }
                continue;
            }
            Ok(Ok(result)) => {
                // Change detected — process OCR lines
                session.frames_changed += 1;
                metrics.record(metric_names::OCR_DONE, result.elapsed_ms * 1000.0);

                let new_lines = result.lines;

                info!(
                    lines = new_lines.len(),
                    mae = result.mae,
                    ocr_ms = result.elapsed_ms,
                    "realtime: change detected"
                );

                // 3. Line-level diff
                let diff = diff_lines(&session.previous_lines, &new_lines);

                info!(
                    added = diff.added.len(),
                    unchanged = diff.unchanged.len(),
                    "realtime line diff"
                );

                session.total_lines_seen += new_lines.len() as u64;
                session.lines_from_cache += diff.unchanged.len() as u64;

                // 4. Translate only added lines
                let mut translate_failed = false;
                for line in &diff.added {
                    if cancel_token.is_cancelled() {
                        break;
                    }

                    // Check session cache first (fast path)
                    if session.line_cache.contains_key(&line.text) {
                        session.lines_from_cache += 1;
                        continue;
                    }

                    // Translate via full pipeline (checks global L1 cache too)
                    let request_id = uuid::Uuid::new_v4().to_string();
                    let noop_chunk = |_chunk: &str| {};

                    let translate_start = Instant::now();
                    match translation_service
                        .translate(&request_id, &line.text, "zh", &cancel_token, &noop_chunk)
                        .await
                    {
                        Ok(tr) => {
                            let translate_us = translate_start.elapsed().as_micros() as f64;
                            metrics.record(metric_names::TRANSLATE_DONE, translate_us);

                            session.line_cache.insert(line.text.clone(), tr.translated_text);
                            session.lines_translated_via_api += 1;
                        }
                        Err(crate::translate::TranslateError::Cancelled) => {
                            info!("realtime translation cancelled");
                            translate_failed = true;
                            break;
                        }
                        Err(e) => {
                            warn!(error = %e, line = &line.text, "realtime line translation failed");
                            // Use original text as fallback
                            session.line_cache.insert(line.text.clone(), line.text.clone());
                            translate_failed = true;
                        }
                    }
                }

                if cancel_token.is_cancelled() {
                    break;
                }

                // 5. Build merged result and render
                let (source_text, translated_text) = session.build_merged(&new_lines);
                let stats = session.stats_json();

                let _ = app_handle.emit("realtime-update", serde_json::json!({
                    "source": source_text,
                    "translated": translated_text,
                    "lines": new_lines.len(),
                    "added": diff.added.len(),
                    "cached": diff.unchanged.len(),
                    "token_saving_pct": session.token_saving_pct(),
                    "stats": stats,
                }));

                // Show result panel if not already visible
                if let Some(window) = app_handle.get_webview_window("result-panel") {
                    let _ = window.show();
                }

                // 6. Update session state
                session.previous_lines = new_lines;

                if translate_failed && !cancel_token.is_cancelled() {
                    // Continue loop even if one line failed
                    debug!("continuing realtime loop despite translation failure");
                }

                metrics.record(
                    metric_names::REALTIME_CYCLE,
                    cycle_start.elapsed().as_micros() as f64,
                );
            }
            Ok(Err(e)) => {
                warn!(error = %e, "realtime OCR failed");
                let _ = app_handle.emit("realtime-error", serde_json::json!({
                    "error": e.to_string(),
                }));
                // Continue loop — transient OCR errors shouldn't stop realtime
            }
            Err(e) => {
                error!(error = %e, "realtime OCR task panicked");
                break;
            }
        }

        // Sleep for remainder of interval
        let elapsed = cycle_start.elapsed();
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
    }

    // Emit final stats
    let final_stats = session.stats_json();
    info!(
        stats = %final_stats,
        "realtime loop stopped"
    );

    let _ = app_handle.emit("realtime-stopped", final_stats);

    // Phase 5: Record realtime session summary to history
    if let Some(ref hs) = history_store {
        if !session.line_cache.is_empty() {
            let (source, translated) = session.build_merged(&session.previous_lines);
            hs.record(HistoryRecord {
                request_id: uuid::Uuid::new_v4().to_string(),
                source_text: source,
                translated_text: translated,
                source_lang: None,
                target_lang: "zh".to_string(),
                mode: "realtime".to_string(),
                tokens_used: 0,
                cached: false,
                created_at: now_unix(),
            });
        }
    }

    // Transition back to Sleep
    state_machine.force_sleep();

    // Reset realtime state in Python worker
    {
        let engine = Arc::clone(&ocr_engine);
        let _ = tokio::task::spawn_blocking(move || engine.reset_realtime()).await;
    }
}

/// Current time as Unix timestamp (seconds).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
