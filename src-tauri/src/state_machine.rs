//! State machine: Sleep → WakeConfirm → ModeSelect → Capture → OCR → Translate → Render → Idle/Sleep
//! Two-stage wake confirmation to reduce false-positive OCR/translation runs.

use std::time::Duration;
use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{info, warn};

/// All possible states in the application lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum AppState {
    Sleep,
    WakeConfirm,
    ModeSelect,
    Capture,
    Ocr,
    Translate,
    Render,
    Idle,
}

impl std::fmt::Display for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppState::Sleep => write!(f, "Sleep"),
            AppState::WakeConfirm => write!(f, "WakeConfirm"),
            AppState::ModeSelect => write!(f, "ModeSelect"),
            AppState::Capture => write!(f, "Capture"),
            AppState::Ocr => write!(f, "Ocr"),
            AppState::Translate => write!(f, "Translate"),
            AppState::Render => write!(f, "Render"),
            AppState::Idle => write!(f, "Idle"),
        }
    }
}

/// Translation mode selected by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum TranslateMode {
    Selection,
    OcrRegion,
    RealtimeIncremental,
}

/// Validated state transitions.
impl AppState {
    /// Returns whether transitioning from `self` to `next` is valid.
    pub fn can_transition_to(self, next: AppState) -> bool {
        matches!(
            (self, next),
            (AppState::Sleep, AppState::WakeConfirm)
                | (AppState::WakeConfirm, AppState::ModeSelect)
                | (AppState::WakeConfirm, AppState::Sleep) // confirmation failed
                | (AppState::ModeSelect, AppState::Capture)
                | (AppState::ModeSelect, AppState::Sleep) // timeout/cancel
                | (AppState::Capture, AppState::Ocr)
                | (AppState::Capture, AppState::Translate) // selection mode skips OCR
                | (AppState::Capture, AppState::Sleep) // cancel
                | (AppState::Ocr, AppState::Translate)
                | (AppState::Ocr, AppState::Sleep) // cancel
                | (AppState::Translate, AppState::Render)
                | (AppState::Translate, AppState::Sleep) // cancel
                | (AppState::Render, AppState::Idle)
                | (AppState::Render, AppState::Sleep) // cancel
                | (AppState::Idle, AppState::Sleep)
                | (AppState::Idle, AppState::ModeSelect) // new request without re-wake
                // Any state can go to Sleep on new wake or force-cancel
                | (_, AppState::Sleep)
        )
    }
}

/// Thread-safe state machine with watch channel for reactive subscribers.
pub struct StateMachine {
    state: RwLock<AppState>,
    mode: RwLock<Option<TranslateMode>>,
    state_tx: watch::Sender<AppState>,
    state_rx: watch::Receiver<AppState>,
}

impl StateMachine {
    pub fn new() -> Self {
        let (state_tx, state_rx) = watch::channel(AppState::Sleep);
        Self {
            state: RwLock::new(AppState::Sleep),
            mode: RwLock::new(None),
            state_tx,
            state_rx,
        }
    }

    /// Current state (non-blocking read).
    pub fn current(&self) -> AppState {
        *self.state.read()
    }

    /// Current translation mode.
    pub fn current_mode(&self) -> Option<TranslateMode> {
        *self.mode.read()
    }

    /// Set translation mode.
    pub fn set_mode(&self, mode: TranslateMode) {
        *self.mode.write() = Some(mode);
        info!(mode = ?mode, "translate_mode_set");
    }

    /// Attempt a state transition. Returns Ok(new_state) or Err with reason.
    pub fn transition(&self, next: AppState) -> Result<AppState, String> {
        let mut state = self.state.write();
        let current = *state;
        if !current.can_transition_to(next) {
            let msg = format!("invalid transition: {} -> {}", current, next);
            warn!("{}", msg);
            return Err(msg);
        }
        *state = next;
        let _ = self.state_tx.send(next);
        info!(from = %current, to = %next, "state_transition");
        Ok(next)
    }

    /// Force transition to Sleep from any state (used on cancel/timeout/new wake).
    pub fn force_sleep(&self) {
        let mut state = self.state.write();
        let prev = *state;
        *state = AppState::Sleep;
        *self.mode.write() = None;
        let _ = self.state_tx.send(AppState::Sleep);
        info!(from = %prev, "force_sleep");
    }

    /// Subscribe to state changes.
    pub fn subscribe(&self) -> watch::Receiver<AppState> {
        self.state_rx.clone()
    }
}

/// Two-stage wake confirmation logic.
/// Stage 1: th_low hit → immediate UI/sound feedback.
/// Stage 2: 150ms accumulative confirmation window, must reach th_high.
pub struct WakeConfirmer {
    /// Low threshold for initial trigger (RMS energy, 0.0-1.0 normalized).
    pub th_low: f32,
    /// High threshold for confirmation.
    pub th_high: f32,
    /// Confirmation window duration.
    pub confirm_window: Duration,
    /// Number of confirmation frames needed within the window.
    pub confirm_frames_needed: u32,
}

impl WakeConfirmer {
    pub fn new() -> Self {
        Self {
            th_low: 0.02,
            th_high: 0.04,
            confirm_window: Duration::from_millis(150),
            confirm_frames_needed: 2,
        }
    }

    /// Check if initial wake should trigger (stage 1).
    /// Returns true if energy exceeds th_low.
    #[inline]
    pub fn should_trigger(&self, wake_score: f32) -> bool {
        wake_score >= self.th_low
    }

    /// Check if confirmation is reached (stage 2).
    /// `scores` are wake scores collected during the confirmation window.
    pub fn is_confirmed(&self, scores: &[f32]) -> bool {
        let hits = scores.iter().filter(|&&s| s >= self.th_high).count() as u32;
        hits >= self.confirm_frames_needed
    }
}
