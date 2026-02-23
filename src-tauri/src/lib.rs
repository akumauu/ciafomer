//! Ciallo: Voice wake + multi-mode translation desktop assistant.
//! Main library: Tauri app setup, command registration, worker spawning.

pub mod state_machine;
pub mod scheduler;
pub mod cancellation;
pub mod metrics;
pub mod audio;
pub mod capture;
pub mod ocr;
pub mod translate;
pub mod realtime;
pub mod history;

use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tauri::Manager;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use state_machine::StateMachine;
use scheduler::{Scheduler, P1Task, P2Task, OcrRoi};
use cancellation::CancelCoordinator;
use metrics::{MetricsRegistry, MetricSummary};
use translate::cache::TranslationCache;
use translate::sqlite_cache::SqliteCache;
use translate::glossary::Glossary;
use translate::deepseek::DeepSeekClient;
use translate::TranslationService;
use capture::{TextCapture, ClipboardCapture};
use capture::screen::ScreenCapture;
use ocr::{OcrEngine, PythonOcrEngine};
use history::{HistoryStore, HistoryRecord};

/// Shared application state managed by Tauri.
pub struct AppContext {
    pub state_machine: Arc<StateMachine>,
    pub scheduler: Arc<Scheduler>,
    pub cancel: Arc<CancelCoordinator>,
    pub metrics: Arc<MetricsRegistry>,
    pub translation_service: Option<Arc<TranslationService>>,
    /// OCR engine (Python worker IPC) — trait object for P2 queue.
    pub ocr_engine: Option<Arc<dyn OcrEngine>>,
    /// Concrete PythonOcrEngine for realtime methods (realtime_ocr, reset_realtime).
    pub python_ocr: Option<Arc<PythonOcrEngine>>,
    /// Screen capture utility.
    pub screen_capture: Arc<ScreenCapture>,
    /// Cached screenshot bytes for OCR region / realtime region selection.
    pub screenshot_cache: parking_lot::Mutex<Option<Vec<u8>>>,
    /// Phase 4: Cancellation token for the realtime incremental loop.
    pub realtime_cancel: parking_lot::Mutex<Option<CancellationToken>>,
    /// Phase 5: History store (async batch writer to SQLite).
    pub history_store: Option<Arc<HistoryStore>>,
}

// --- Tauri Commands ---

#[tauri::command]
fn get_state(ctx: tauri::State<'_, AppContext>) -> String {
    format!("{}", ctx.state_machine.current())
}

#[tauri::command]
fn get_metrics_summary(ctx: tauri::State<'_, AppContext>) -> HashMap<String, MetricSummary> {
    ctx.metrics.summary()
}

#[tauri::command]
fn select_mode(
    ctx: tauri::State<'_, AppContext>,
    app: tauri::AppHandle,
    mode: String,
) -> Result<String, String> {
    let translate_mode = match mode.as_str() {
        "selection" => state_machine::TranslateMode::Selection,
        "ocr_region" => state_machine::TranslateMode::OcrRegion,
        "realtime" => state_machine::TranslateMode::RealtimeIncremental,
        _ => return Err(format!("unknown mode: {mode}")),
    };
    ctx.state_machine.set_mode(translate_mode);
    ctx.state_machine
        .transition(state_machine::AppState::Capture)
        .map_err(|e| e)?;

    // Hide mode panel after selection
    if let Some(w) = app.get_webview_window("mode-panel") {
        let _ = w.hide();
    }

    match translate_mode {
        state_machine::TranslateMode::Selection => {
            // For selection mode, immediately submit capture task to P1 (non-blocking)
            let request_id = uuid::Uuid::new_v4().to_string();
            let (_, generation) = ctx.cancel.p1.cancel_and_advance();
            let p1_tx = ctx.scheduler.p1_sender();
            let _ = p1_tx.try_send(P1Task::CaptureSelection {
                request_id,
                generation,
                enqueued_at: Instant::now(),
            });
        }
        state_machine::TranslateMode::OcrRegion => {
            // For OCR region mode:
            // 1. Capture screenshot
            // 2. Store in cache
            // 3. Show capture overlay window
            match ctx.screen_capture.capture() {
                Ok(png_bytes) => {
                    info!(size = png_bytes.len(), "screenshot captured for OCR");
                    *ctx.screenshot_cache.lock() = Some(png_bytes);

                    if let Some(w) = app.get_webview_window("capture-overlay") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
                Err(e) => {
                    warn!(error = %e, "screen capture failed");
                    use tauri::Emitter;
                    let _ = app.emit(
                        "ocr-error",
                        serde_json::json!({ "error": format!("Screen capture failed: {e}") }),
                    );
                    ctx.state_machine.force_sleep();
                }
            }
        }
        state_machine::TranslateMode::RealtimeIncremental => {
            // Phase 4: Realtime incremental translation.
            // Same overlay flow as OCR Region: capture screenshot → show overlay → user selects ROI.
            // Then submit_ocr_selection detects realtime mode and starts the loop.
            if ctx.python_ocr.is_none() || ctx.translation_service.is_none() {
                warn!("realtime mode requires OCR engine and translation service");
                ctx.state_machine.force_sleep();
                return Ok(format!("{}", ctx.state_machine.current()));
            }

            match ctx.screen_capture.capture() {
                Ok(png_bytes) => {
                    info!(size = png_bytes.len(), "screenshot captured for realtime region selection");
                    *ctx.screenshot_cache.lock() = Some(png_bytes);

                    if let Some(w) = app.get_webview_window("capture-overlay") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
                Err(e) => {
                    warn!(error = %e, "screen capture failed for realtime");
                    use tauri::Emitter;
                    let _ = app.emit(
                        "realtime-error",
                        serde_json::json!({ "error": format!("Screen capture failed: {e}") }),
                    );
                    ctx.state_machine.force_sleep();
                }
            }
        }
    }

    Ok(format!("{}", ctx.state_machine.current()))
}

/// Return the cached screenshot as base64-encoded PNG.
/// Called by the capture-overlay window to display the screenshot.
#[tauri::command]
fn get_screenshot_base64(ctx: tauri::State<'_, AppContext>) -> Result<String, String> {
    let cache = ctx.screenshot_cache.lock();
    match cache.as_ref() {
        Some(bytes) => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(encoded)
        }
        None => Err("no screenshot available".into()),
    }
}

/// Submit an OCR region selection from the capture overlay.
/// For OcrRegion mode: submits to P2 queue for single OCR + translation.
/// For RealtimeIncremental mode: starts the realtime loop with the selected ROI.
#[tauri::command]
fn submit_ocr_selection(
    ctx: tauri::State<'_, AppContext>,
    app: tauri::AppHandle,
    roi_type: String,
    roi_params: serde_json::Value,
) -> Result<String, String> {
    // Hide the overlay
    if let Some(w) = app.get_webview_window("capture-overlay") {
        let _ = w.hide();
    }

    let current_mode = ctx.state_machine.current_mode();

    // Check if this is realtime mode
    if current_mode == Some(state_machine::TranslateMode::RealtimeIncremental) {
        // Phase 4: Start realtime loop (don't consume screenshot cache — loop captures its own)
        *ctx.screenshot_cache.lock() = None;

        let python_ocr = ctx.python_ocr.as_ref()
            .ok_or_else(|| "OCR engine not available".to_string())?
            .clone();
        let translation_service = ctx.translation_service.as_ref()
            .ok_or_else(|| "translation service not available".to_string())?
            .clone();

        // Cancel any existing realtime loop
        if let Some(old_token) = ctx.realtime_cancel.lock().take() {
            old_token.cancel();
        }

        // Create new cancellation token for the realtime loop
        let cancel_token = CancellationToken::new();
        *ctx.realtime_cancel.lock() = Some(cancel_token.clone());

        let screen_capture = Arc::clone(&ctx.screen_capture);
        let metrics = Arc::clone(&ctx.metrics);
        let state_machine = Arc::clone(&ctx.state_machine);
        let history = ctx.history_store.clone();
        let handle = app.clone();

        tokio::spawn(realtime::run_realtime_loop(
            python_ocr,
            screen_capture,
            translation_service,
            cancel_token,
            roi_type.clone(),
            roi_params.clone(),
            handle,
            metrics,
            state_machine,
            history,
        ));

        info!(roi_type = %roi_type, "realtime loop started");
        return Ok("realtime_started".to_string());
    }

    // Normal OCR Region mode: submit to P2 queue
    let image_data = ctx
        .screenshot_cache
        .lock()
        .take()
        .ok_or_else(|| "no screenshot cached".to_string())?;

    // Parse ROI
    let roi = match roi_type.as_str() {
        "rect" => {
            let x = roi_params["x"].as_u64().unwrap_or(0) as u32;
            let y = roi_params["y"].as_u64().unwrap_or(0) as u32;
            let w = roi_params["w"].as_u64().unwrap_or(0) as u32;
            let h = roi_params["h"].as_u64().unwrap_or(0) as u32;
            OcrRoi::Rect { x, y, w, h }
        }
        "polygon" => {
            let points = roi_params["points"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            let a = p.as_array()?;
                            Some((a.first()?.as_u64()? as u32, a.get(1)?.as_u64()? as u32))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            OcrRoi::Polygon { points }
        }
        "perspective" => {
            let corners_vec = roi_params["corners"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            let a = p.as_array()?;
                            Some((a.first()?.as_u64()? as u32, a.get(1)?.as_u64()? as u32))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if corners_vec.len() != 4 {
                return Err("perspective requires exactly 4 corners".into());
            }
            let corners: [(u32, u32); 4] = [
                corners_vec[0],
                corners_vec[1],
                corners_vec[2],
                corners_vec[3],
            ];
            OcrRoi::Perspective { corners }
        }
        _ => return Err(format!("unknown roi_type: {roi_type}")),
    };

    // Submit to P2 queue
    let request_id = uuid::Uuid::new_v4().to_string();
    let (_, generation) = ctx.cancel.p2.cancel_and_advance();
    let p2_tx = ctx.scheduler.p2_sender();
    let _ = p2_tx.try_send(P2Task::OcrRegion {
        request_id: request_id.clone(),
        generation,
        image_data,
        roi,
        enqueued_at: Instant::now(),
    });

    info!(request_id = %request_id, roi_type = %roi_type, "OCR region submitted to P2");
    Ok(request_id)
}

/// Cancel OCR capture (user pressed Escape or Cancel in overlay).
#[tauri::command]
fn cancel_ocr_capture(ctx: tauri::State<'_, AppContext>, app: tauri::AppHandle) {
    // Hide overlay
    if let Some(w) = app.get_webview_window("capture-overlay") {
        let _ = w.hide();
    }
    // Clear cached screenshot
    *ctx.screenshot_cache.lock() = None;
    // Return to sleep
    ctx.state_machine.force_sleep();
    info!("OCR capture cancelled");
}

/// Phase 4: Stop the realtime incremental translation loop.
#[tauri::command]
fn stop_realtime(ctx: tauri::State<'_, AppContext>) {
    if let Some(token) = ctx.realtime_cancel.lock().take() {
        token.cancel();
        info!("realtime loop stop requested");
    }
    ctx.state_machine.force_sleep();
}

#[tauri::command]
fn cancel_current(ctx: tauri::State<'_, AppContext>) {
    // Also cancel realtime loop if running
    if let Some(token) = ctx.realtime_cancel.lock().take() {
        token.cancel();
    }
    ctx.scheduler.preempt_for_wake();
    ctx.state_machine.force_sleep();
}

#[tauri::command]
fn dismiss(ctx: tauri::State<'_, AppContext>, app: tauri::AppHandle) {
    // Also cancel realtime loop if running
    if let Some(token) = ctx.realtime_cancel.lock().take() {
        token.cancel();
    }
    ctx.state_machine.force_sleep();
    if let Some(w) = app.get_webview_window("mode-panel") {
        let _ = w.hide();
    }
    if let Some(w) = app.get_webview_window("result-panel") {
        let _ = w.hide();
    }
    if let Some(w) = app.get_webview_window("capture-overlay") {
        let _ = w.hide();
    }
}

/// Phase 5: Query recent translation history.
#[tauri::command]
fn get_history(
    ctx: tauri::State<'_, AppContext>,
    limit: Option<usize>,
) -> Vec<HistoryRecord> {
    match &ctx.history_store {
        Some(store) => store.query_recent(limit.unwrap_or(50)),
        None => Vec::new(),
    }
}

/// Build and run the Tauri application.
pub fn run() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ciallo=debug,tauri=info".parse().unwrap()),
        )
        .with_target(true)
        .with_thread_ids(true)
        .init();

    info!("ciallo starting");

    let cancel = Arc::new(CancelCoordinator::new());
    let metrics = Arc::new(MetricsRegistry::new());
    let state_machine = Arc::new(StateMachine::new());
    let scheduler = Arc::new(Scheduler::new(Arc::clone(&cancel), Arc::clone(&metrics)));

    // Load glossary
    let glossary_path = std::path::Path::new("glossary/default.json");
    let glossary = Arc::new(
        Glossary::load_from_file(glossary_path).unwrap_or_else(|e| {
            warn!(error = %e, "glossary load failed, using empty");
            Glossary::empty()
        }),
    );

    // Create translation cache (512 entries, 10min TTL)
    let cache = Arc::new(TranslationCache::new(512, Duration::from_secs(600)));

    // Phase 5: Create L2 SQLite translation cache (TTL 7d)
    let data_dir = dirs_data_path();
    let l2_cache: Option<Arc<SqliteCache>> = {
        let db_path = data_dir.join("translation_cache.db");
        match SqliteCache::open(&db_path) {
            Ok(c) => {
                let arc = Arc::new(c);
                SqliteCache::start_cleanup_loop(Arc::clone(&arc));
                info!("L2 SQLite cache initialized");
                Some(arc)
            }
            Err(e) => {
                warn!(error = %e, "L2 SQLite cache init failed, running without persistent cache");
                None
            }
        }
    };

    // Phase 5: Create history store (async batch writer, 300ms flush)
    let history_store: Option<Arc<HistoryStore>> = {
        let db_path = data_dir.join("history.db");
        match HistoryStore::open(&db_path) {
            Ok(store) => {
                info!("history store initialized");
                Some(store)
            }
            Err(e) => {
                warn!(error = %e, "history store init failed, running without history");
                None
            }
        }
    };

    // Create DeepSeek client and translation service
    let translation_service = match DeepSeekClient::new() {
        Ok(client) => {
            info!("DeepSeek API client initialized");
            Some(Arc::new(TranslationService::new(
                client,
                Arc::clone(&cache),
                l2_cache.clone(),
                Arc::clone(&glossary),
            )))
        }
        Err(e) => {
            warn!(error = %e, "DeepSeek client init failed (API key missing?), translation disabled");
            None
        }
    };

    // Create text capture (xdotool + xclip)
    let capture: Arc<dyn TextCapture> = Arc::new(ClipboardCapture::new(
        Duration::from_millis(60),
        Duration::from_millis(200),
    ));

    // Create screen capture
    let screen_capture = Arc::new(ScreenCapture::new());

    // Create OCR engine (Python worker)
    let python_ocr: Option<Arc<PythonOcrEngine>> = {
        let worker_script = std::path::PathBuf::from("../python-worker/worker.py");
        // Try to find Python binary: prefer venv, fallback to system
        let python_bin = if std::path::Path::new("../python-worker/.venv/bin/python3").exists() {
            "../python-worker/.venv/bin/python3".to_string()
        } else {
            "python3".to_string()
        };

        let engine = Arc::new(PythonOcrEngine::new(&python_bin, worker_script));

        // Start health check loop (30s interval)
        PythonOcrEngine::start_health_loop(Arc::clone(&engine), Duration::from_secs(30));

        info!(python = %python_bin, "Python OCR engine configured");
        Some(engine)
    };

    // Create trait-object OCR engine from PythonOcrEngine
    let ocr_engine: Option<Arc<dyn OcrEngine>> = python_ocr
        .as_ref()
        .map(|e| Arc::clone(e) as Arc<dyn OcrEngine>);

    let app_context = AppContext {
        state_machine: Arc::clone(&state_machine),
        scheduler: Arc::clone(&scheduler),
        cancel: Arc::clone(&cancel),
        metrics: Arc::clone(&metrics),
        translation_service: translation_service.clone(),
        ocr_engine: ocr_engine.clone(),
        python_ocr: python_ocr.clone(),
        screen_capture,
        screenshot_cache: parking_lot::Mutex::new(None),
        realtime_cancel: parking_lot::Mutex::new(None),
        history_store: history_store.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(app_context)
        .setup(move |app| {
            let handle = app.handle().clone();

            // Start P0 handler loop (dedicated OS thread)
            let p0_rx = scheduler.p0_receiver();
            scheduler::run_p0_loop(
                p0_rx,
                Arc::clone(&state_machine),
                Arc::clone(&metrics),
                handle.clone(),
            );

            // Start P1 worker loop (Tokio task) if translation service is available
            if let Some(ref ts) = translation_service {
                scheduler::run_p1_loop(
                    Arc::clone(&scheduler),
                    Arc::clone(&state_machine),
                    Arc::clone(&cancel),
                    Arc::clone(&metrics),
                    Arc::clone(ts),
                    Arc::clone(&capture),
                    history_store.clone(),
                    handle.clone(),
                );
                info!("P1 worker loop started");
            } else {
                warn!("P1 worker loop not started (no translation service)");
            }

            // Start P2 worker loop (Tokio task) if both OCR engine and translation service are available
            if let (Some(ref engine), Some(ref ts)) = (&ocr_engine, &translation_service) {
                scheduler::run_p2_loop(
                    Arc::clone(&scheduler),
                    Arc::clone(&state_machine),
                    Arc::clone(&cancel),
                    Arc::clone(&metrics),
                    Arc::clone(engine),
                    Arc::clone(ts),
                    Arc::clone(&capture),
                    handle.clone(),
                );
                info!("P2 worker loop started");
            } else {
                warn!("P2 worker loop not started (missing OCR engine or translation service)");
            }

            // Start audio pipeline
            let audio_config = audio::AudioConfig::default();
            match audio::start_audio_pipeline(
                audio_config,
                Arc::clone(&scheduler),
                Arc::clone(&state_machine),
                Arc::clone(&metrics),
            ) {
                Ok(_handle) => {
                    info!("audio pipeline started");
                }
                Err(e) => {
                    warn!(error = %e, "audio pipeline failed to start (may not have mic access)");
                }
            }

            info!("ciallo setup complete");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_metrics_summary,
            select_mode,
            cancel_current,
            dismiss,
            get_screenshot_base64,
            submit_ocr_selection,
            cancel_ocr_capture,
            stop_realtime,
            get_history,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ciallo");
}

/// Determine the data directory for persistent storage (SQLite DBs).
/// Uses XDG_DATA_HOME on Linux, falls back to ~/.local/share/ciallo.
fn dirs_data_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".local/share")
        });
    let dir = base.join("ciallo");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
