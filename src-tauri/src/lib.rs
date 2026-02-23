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

use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tauri::Manager;
use tracing::{info, warn};

use state_machine::StateMachine;
use scheduler::{Scheduler, P1Task};
use cancellation::CancelCoordinator;
use metrics::{MetricsRegistry, MetricSummary};
use translate::cache::TranslationCache;
use translate::glossary::Glossary;
use translate::deepseek::DeepSeekClient;
use translate::TranslationService;
use capture::{TextCapture, ClipboardCapture};

/// Shared application state managed by Tauri.
pub struct AppContext {
    pub state_machine: Arc<StateMachine>,
    pub scheduler: Arc<Scheduler>,
    pub cancel: Arc<CancelCoordinator>,
    pub metrics: Arc<MetricsRegistry>,
    pub translation_service: Option<Arc<TranslationService>>,
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
fn select_mode(ctx: tauri::State<'_, AppContext>, mode: String) -> Result<String, String> {
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

    // For selection mode, immediately submit capture task to P1 (non-blocking)
    if translate_mode == state_machine::TranslateMode::Selection {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (_, generation) = ctx.cancel.p1.cancel_and_advance();
        let p1_tx = ctx.scheduler.p1_sender();
        let _ = p1_tx.try_send(P1Task::CaptureSelection {
            request_id,
            generation,
            enqueued_at: Instant::now(),
        });
    }

    Ok(format!("{}", ctx.state_machine.current()))
}

#[tauri::command]
fn cancel_current(ctx: tauri::State<'_, AppContext>) {
    ctx.scheduler.preempt_for_wake();
    ctx.state_machine.force_sleep();
}

#[tauri::command]
fn dismiss(ctx: tauri::State<'_, AppContext>, app: tauri::AppHandle) {
    ctx.state_machine.force_sleep();
    if let Some(w) = app.get_webview_window("mode-panel") {
        let _ = w.hide();
    }
    if let Some(w) = app.get_webview_window("result-panel") {
        let _ = w.hide();
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

    // Create DeepSeek client and translation service
    let translation_service = match DeepSeekClient::new() {
        Ok(client) => {
            info!("DeepSeek API client initialized");
            Some(Arc::new(TranslationService::new(
                client,
                Arc::clone(&cache),
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

    let app_context = AppContext {
        state_machine: Arc::clone(&state_machine),
        scheduler: Arc::clone(&scheduler),
        cancel: Arc::clone(&cancel),
        metrics: Arc::clone(&metrics),
        translation_service: translation_service.clone(),
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
                    handle.clone(),
                );
                info!("P1 worker loop started");
            } else {
                warn!("P1 worker loop not started (no translation service)");
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running ciallo");
}
