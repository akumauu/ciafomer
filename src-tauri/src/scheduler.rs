//! Three-queue scheduler: P0 (Wake/UI), P1 (Capture/Translate), P2 (OCR Heavy).
//! P0 is an unbounded crossbeam channel for zero-latency wake handling.
//! P1 uses Tokio async tasks. P2 uses spawn_blocking / dedicated thread pool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use crossbeam_channel as cb;
use tauri::Manager;
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use crate::cancellation::CancelCoordinator;
use crate::capture::TextCapture;
use crate::metrics::{MetricsRegistry, metric_names};
use crate::state_machine::{AppState, StateMachine};
use crate::translate::TranslationService;

/// P0 task: wake event, UI feedback, sound trigger.
/// Must be processed with < 1ms computation. No network, no OCR, no disk sync.
#[derive(Debug)]
pub enum P0Task {
    WakeDetected {
        wake_score: f32,
        timestamp: Instant,
    },
    WakeConfirmed {
        timestamp: Instant,
    },
    WakeRejected,
    ShowModePanel,
    HideModePanel,
    PlaySound {
        sound_id: &'static str,
    },
    ForceCancel,
}

/// P1 task: capture, translate, render — async-friendly work.
#[derive(Debug)]
pub enum P1Task {
    CaptureSelection {
        request_id: String,
        generation: u64,
        enqueued_at: Instant,
    },
    Translate {
        request_id: String,
        generation: u64,
        text: String,
        target_lang: String,
        enqueued_at: Instant,
    },
    RenderResult {
        request_id: String,
        generation: u64,
        source: String,
        translated: String,
        enqueued_at: Instant,
    },
}

/// P2 task: heavy OCR work, runs on blocking thread pool / OCR engine.
#[derive(Debug)]
pub enum P2Task {
    OcrRegion {
        request_id: String,
        generation: u64,
        image_data: Vec<u8>,
        roi: OcrRoi,
        enqueued_at: Instant,
    },
}

/// Region of interest for OCR.
#[derive(Debug, Clone)]
pub enum OcrRoi {
    Rect { x: u32, y: u32, w: u32, h: u32 },
    Polygon { points: Vec<(u32, u32)> },
    Perspective { corners: [(u32, u32); 4] },
}

/// The scheduler owns channels and dispatches tasks to the appropriate queue.
pub struct Scheduler {
    // P0: crossbeam unbounded (never blocks sender)
    p0_tx: cb::Sender<P0Task>,
    p0_rx: cb::Receiver<P0Task>,

    // P1: tokio mpsc bounded — Mutex for interior mutability (Scheduler behind Arc)
    p1_tx: mpsc::Sender<P1Task>,
    p1_rx: parking_lot::Mutex<Option<mpsc::Receiver<P1Task>>>,

    // P2: tokio mpsc bounded
    p2_tx: mpsc::Sender<P2Task>,
    p2_rx: parking_lot::Mutex<Option<mpsc::Receiver<P2Task>>>,

    cancel: Arc<CancelCoordinator>,
    metrics: Arc<MetricsRegistry>,
}

impl Scheduler {
    pub fn new(cancel: Arc<CancelCoordinator>, metrics: Arc<MetricsRegistry>) -> Self {
        let (p0_tx, p0_rx) = cb::unbounded();
        let (p1_tx, p1_rx) = mpsc::channel(64);
        let (p2_tx, p2_rx) = mpsc::channel(16);

        Self {
            p0_tx,
            p0_rx,
            p1_tx,
            p1_rx: parking_lot::Mutex::new(Some(p1_rx)),
            p2_tx,
            p2_rx: parking_lot::Mutex::new(Some(p2_rx)),
            cancel,
            metrics,
        }
    }

    /// Submit a P0 task (never blocks, highest priority).
    pub fn submit_p0(&self, task: P0Task) {
        let _ = self.p0_tx.send(task);
    }

    /// Submit a P1 task. Returns false if queue is full (back-pressure).
    pub async fn submit_p1(&self, task: P1Task) -> bool {
        match self.p1_tx.try_send(task) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(task)) => {
                warn!("P1 queue full, awaiting slot");
                self.p1_tx.send(task).await.is_ok()
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("P1 queue closed");
                false
            }
        }
    }

    /// Submit a P2 task. Returns false if queue is full.
    pub async fn submit_p2(&self, task: P2Task) -> bool {
        match self.p2_tx.try_send(task) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(task)) => {
                warn!("P2 queue full, awaiting slot");
                self.p2_tx.send(task).await.is_ok()
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("P2 queue closed");
                false
            }
        }
    }

    /// Take the P0 receiver (can only be called once, by the P0 handler loop).
    pub fn p0_receiver(&self) -> cb::Receiver<P0Task> {
        self.p0_rx.clone()
    }

    /// Take the P1 receiver (call once to start the P1 worker loop).
    pub fn take_p1_receiver(&self) -> Option<mpsc::Receiver<P1Task>> {
        self.p1_rx.lock().take()
    }

    /// Take the P2 receiver (call once to start the P2 worker loop).
    pub fn take_p2_receiver(&self) -> Option<mpsc::Receiver<P2Task>> {
        self.p2_rx.lock().take()
    }

    /// Preemption: new wake arrived — cancel all P1/P2 cancellable tasks.
    pub fn preempt_for_wake(&self) {
        let gen = self.cancel.cancel_all_and_advance();
        info!(new_global_generation = gen, "preempt_for_wake: cancelled all P1/P2");
    }

    pub fn cancel_coordinator(&self) -> &Arc<CancelCoordinator> {
        &self.cancel
    }

    pub fn metrics(&self) -> &Arc<MetricsRegistry> {
        &self.metrics
    }

    /// Get P1 sender clone (for use across async boundaries).
    pub fn p1_sender(&self) -> mpsc::Sender<P1Task> {
        self.p1_tx.clone()
    }

    /// Get P2 sender clone.
    pub fn p2_sender(&self) -> mpsc::Sender<P2Task> {
        self.p2_tx.clone()
    }
}

/// P0 handler loop: runs on a dedicated OS thread (not Tokio).
/// Processes wake events, UI feedback, and sound triggers.
/// MUST NOT do any heavy computation (>1ms), network I/O, or disk sync.
pub fn run_p0_loop(
    rx: cb::Receiver<P0Task>,
    state_machine: Arc<StateMachine>,
    metrics: Arc<MetricsRegistry>,
    app_handle: tauri::AppHandle,
) {
    std::thread::Builder::new()
        .name("p0-handler".into())
        .spawn(move || {
            loop {
                match rx.recv() {
                    Ok(task) => {
                        handle_p0_task(task, &state_machine, &metrics, &app_handle);
                    }
                    Err(cb::RecvError) => {
                        info!("P0 channel closed, exiting handler loop");
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn P0 handler thread");
}

fn handle_p0_task(
    task: P0Task,
    state_machine: &Arc<StateMachine>,
    metrics: &Arc<MetricsRegistry>,
    app: &tauri::AppHandle,
) {
    use tauri::Emitter;
    match task {
        P0Task::WakeDetected { wake_score, timestamp } => {
            let latency_us = timestamp.elapsed().as_micros() as f64;
            metrics.record(metric_names::QUEUE_WAIT_P0, latency_us);

            let _ = state_machine.transition(AppState::WakeConfirm);
            let _ = app.emit("wake-detected", serde_json::json!({
                "score": wake_score,
                "timestamp_us": latency_us,
            }));
            let ui_latency = timestamp.elapsed().as_micros() as f64;
            metrics.record(metric_names::WAKE_UI_EMITTED, ui_latency);
            tracing::info!(
                wake_score = wake_score,
                latency_us = ui_latency,
                "wake_ui_emitted"
            );
        }
        P0Task::WakeConfirmed { timestamp } => {
            let _ = state_machine.transition(AppState::ModeSelect);
            let _ = app.emit("wake-confirmed", ());

            if let Some(window) = app.get_webview_window("mode-panel") {
                let _ = window.show();
                let _ = window.set_focus();
            }
            let panel_latency = timestamp.elapsed().as_micros() as f64;
            metrics.record(metric_names::MODE_PANEL_VISIBLE, panel_latency);
        }
        P0Task::WakeRejected => {
            state_machine.force_sleep();
            let _ = app.emit("wake-rejected", ());
        }
        P0Task::ShowModePanel => {
            if let Some(window) = app.get_webview_window("mode-panel") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        P0Task::HideModePanel => {
            if let Some(window) = app.get_webview_window("mode-panel") {
                let _ = window.hide();
            }
        }
        P0Task::PlaySound { sound_id } => {
            let _ = app.emit("play-sound", sound_id);
        }
        P0Task::ForceCancel => {
            state_machine.force_sleep();
            let _ = app.emit("force-cancel", ());
            if let Some(window) = app.get_webview_window("mode-panel") {
                let _ = window.hide();
            }
            if let Some(window) = app.get_webview_window("result-panel") {
                let _ = window.hide();
            }
        }
    }
}

/// P1 worker loop: processes the Capture → Translate → Render pipeline.
/// Runs as a Tokio task. Each stage checks GenerationGuard before proceeding.
pub fn run_p1_loop(
    scheduler: Arc<Scheduler>,
    state_machine: Arc<StateMachine>,
    cancel: Arc<CancelCoordinator>,
    metrics: Arc<MetricsRegistry>,
    translation_service: Arc<TranslationService>,
    capture: Arc<dyn TextCapture>,
    app_handle: tauri::AppHandle,
) {
    let mut rx = scheduler
        .take_p1_receiver()
        .expect("P1 receiver already taken");

    let p1_tx = scheduler.p1_sender();

    tokio::spawn(async move {
        info!("P1 worker loop started");

        while let Some(task) = rx.recv().await {
            match task {
                P1Task::CaptureSelection {
                    request_id,
                    generation,
                    enqueued_at,
                } => {
                    let wait_us = enqueued_at.elapsed().as_micros() as f64;
                    metrics.record(metric_names::QUEUE_WAIT_P1, wait_us);

                    let guard = cancel.p1_guard();
                    if !guard.should_continue() {
                        continue;
                    }

                    let capture_start = Instant::now();
                    let capture_clone = Arc::clone(&capture);
                    let capture_result =
                        tokio::task::spawn_blocking(move || capture_clone.capture_selection())
                            .await;

                    if !guard.should_continue() {
                        continue;
                    }

                    match capture_result {
                        Ok(Ok(text)) => {
                            let capture_us = capture_start.elapsed().as_micros() as f64;
                            metrics.record(metric_names::CAPTURE_DONE, capture_us);

                            {
                                use tauri::Emitter;
                                let _ = app_handle.emit(
                                    "capture-complete",
                                    serde_json::json!({ "text": &text }),
                                );
                            }

                            let _ = state_machine.transition(AppState::Translate);

                            let _ = p1_tx
                                .send(P1Task::Translate {
                                    request_id,
                                    generation,
                                    text,
                                    target_lang: "zh".to_string(),
                                    enqueued_at: Instant::now(),
                                })
                                .await;
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "capture failed");
                            use tauri::Emitter;
                            let _ = app_handle.emit(
                                "capture-error",
                                serde_json::json!({ "error": e.to_string() }),
                            );
                            state_machine.force_sleep();
                        }
                        Err(e) => {
                            error!(error = %e, "capture task panicked");
                            state_machine.force_sleep();
                        }
                    }
                }

                P1Task::Translate {
                    request_id,
                    generation,
                    text,
                    target_lang,
                    enqueued_at,
                } => {
                    let wait_us = enqueued_at.elapsed().as_micros() as f64;
                    metrics.record(metric_names::QUEUE_WAIT_P1, wait_us);

                    let guard = cancel.p1_guard();
                    if !guard.should_continue() {
                        continue;
                    }

                    let translate_start = Instant::now();
                    let first_chunk_done = Arc::new(AtomicBool::new(false));
                    let first_chunk_clone = Arc::clone(&first_chunk_done);
                    let metrics_clone = Arc::clone(&metrics);
                    let app_clone = app_handle.clone();

                    let on_chunk = move |chunk: &str| {
                        if !first_chunk_clone.swap(true, Ordering::Relaxed) {
                            let elapsed = translate_start.elapsed().as_micros() as f64;
                            metrics_clone.record(metric_names::TRANSLATE_FIRST_CHUNK, elapsed);
                        }
                        use tauri::Emitter;
                        let _ = app_clone.emit("translate-chunk", chunk);
                    };

                    let result = translation_service
                        .translate(
                            &request_id,
                            &text,
                            &target_lang,
                            guard.token(),
                            &on_chunk,
                        )
                        .await;

                    if !guard.should_continue() {
                        continue;
                    }

                    match result {
                        Ok(translate_result) => {
                            let translate_us = translate_start.elapsed().as_micros() as f64;
                            metrics.record(metric_names::TRANSLATE_DONE, translate_us);

                            let _ = state_machine.transition(AppState::Render);

                            let _ = p1_tx
                                .send(P1Task::RenderResult {
                                    request_id,
                                    generation,
                                    source: text,
                                    translated: translate_result.translated_text,
                                    enqueued_at: Instant::now(),
                                })
                                .await;
                        }
                        Err(crate::translate::TranslateError::Cancelled) => {
                            info!("translation cancelled");
                        }
                        Err(e) => {
                            warn!(error = %e, "translation failed");
                            use tauri::Emitter;
                            let _ = app_handle.emit(
                                "translate-error",
                                serde_json::json!({ "error": e.to_string() }),
                            );
                            state_machine.force_sleep();
                        }
                    }
                }

                P1Task::RenderResult {
                    request_id,
                    generation: _,
                    source,
                    translated,
                    enqueued_at,
                } => {
                    let wait_us = enqueued_at.elapsed().as_micros() as f64;
                    metrics.record(metric_names::QUEUE_WAIT_P1, wait_us);

                    let guard = cancel.p1_guard();
                    if !guard.should_continue() {
                        continue;
                    }

                    let render_start = Instant::now();

                    {
                        use tauri::Emitter;
                        let _ = app_handle.emit(
                            "translate-complete",
                            serde_json::json!({
                                "request_id": request_id,
                                "source": source,
                                "translated": translated,
                            }),
                        );
                    }

                    if let Some(window) = app_handle.get_webview_window("result-panel") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }

                    let render_us = render_start.elapsed().as_micros() as f64;
                    metrics.record(metric_names::RENDER_DONE, render_us);

                    let _ = state_machine.transition(AppState::Idle);
                }
            }
        }

        info!("P1 worker loop exiting");
    });
}
