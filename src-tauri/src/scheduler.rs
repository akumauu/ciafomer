//! Three-queue scheduler: P0 (Wake/UI), P1 (Capture/Translate), P2 (OCR Heavy).
//! P0 is an unbounded crossbeam channel for zero-latency wake handling.
//! P1 uses Tokio async tasks. P2 uses spawn_blocking / dedicated thread pool.

use std::sync::Arc;
use std::time::Instant;
use crossbeam_channel as cb;
use tauri::Manager;
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use crate::cancellation::CancelCoordinator;
use crate::metrics::{MetricsRegistry, metric_names};
use crate::state_machine::StateMachine;

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
        enqueued_at: Instant,
    },
    RenderResult {
        request_id: String,
        generation: u64,
        translated: String,
        enqueued_at: Instant,
    },
}

/// P2 task: heavy OCR work, runs on blocking thread pool / Python worker.
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

    // P1: tokio mpsc bounded
    p1_tx: mpsc::Sender<P1Task>,
    p1_rx: Option<mpsc::Receiver<P1Task>>,

    // P2: tokio mpsc bounded
    p2_tx: mpsc::Sender<P2Task>,
    p2_rx: Option<mpsc::Receiver<P2Task>>,

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
            p1_rx: Some(p1_rx),
            p2_tx,
            p2_rx: Some(p2_rx),
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
    pub fn take_p1_receiver(&mut self) -> Option<mpsc::Receiver<P1Task>> {
        self.p1_rx.take()
    }

    /// Take the P2 receiver (call once to start the P2 worker loop).
    pub fn take_p2_receiver(&mut self) -> Option<mpsc::Receiver<P2Task>> {
        self.p2_rx.take()
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

            // Stage 1: immediate feedback
            let _ = state_machine.transition(crate::state_machine::AppState::WakeConfirm);
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
            let _ = state_machine.transition(crate::state_machine::AppState::ModeSelect);
            let _ = app.emit("wake-confirmed", ());

            // Show mode panel
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
        }
    }
}
