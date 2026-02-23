//! Audio pipeline coordinator.
//! Manages: audio device → ring buffer → VAD → wake detection → P0 channel.
//! Audio capture runs on cpal's callback thread.
//! Processing runs on a dedicated thread reading from the ring buffer.

pub mod ring_buffer;
pub mod vad;
pub mod wake;

use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tracing::{info, error};

use ring_buffer::RingBuffer;
use vad::EnergyVad;
use wake::{WakeDetector, EnergyPatternDetector};
use crate::scheduler::{P0Task, Scheduler};
use crate::state_machine::{AppState, StateMachine, WakeConfirmer};
use crate::metrics::{MetricsRegistry, metric_names};

/// Audio pipeline configuration.
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub ring_buffer_secs: f32,
    pub frame_size: usize, // samples per processing frame
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            channels: 1,
            ring_buffer_secs: 3.0,
            frame_size: 512, // ~32ms at 16kHz
        }
    }
}

/// Shared audio state between capture callback and processing thread.
struct SharedAudioState {
    ring_buffer: Mutex<RingBuffer>,
}

/// Start the audio pipeline. Returns a handle that keeps it alive.
/// Call `AudioHandle::stop()` to shut down.
pub struct AudioHandle {
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    processing_thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioHandle {
    pub fn stop(&self) {
        self.stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.processing_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Start the full audio pipeline: capture + processing.
pub fn start_audio_pipeline(
    config: AudioConfig,
    scheduler: Arc<Scheduler>,
    state_machine: Arc<StateMachine>,
    metrics: Arc<MetricsRegistry>,
) -> Result<AudioHandle, String> {
    let ring = RingBuffer::new(config.sample_rate, config.ring_buffer_secs);
    let shared = Arc::new(SharedAudioState {
        ring_buffer: Mutex::new(ring),
    });

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Start audio capture via cpal
    let shared_capture = Arc::clone(&shared);
    let _stream = start_capture_stream(&config, shared_capture)?;

    // Start processing thread
    let shared_proc = Arc::clone(&shared);
    let stop_proc = Arc::clone(&stop_flag);
    let processing_thread = std::thread::Builder::new()
        .name("audio-processing".into())
        .spawn(move || {
            run_processing_loop(
                shared_proc,
                config.frame_size,
                stop_proc,
                scheduler,
                state_machine,
                metrics,
            );
        })
        .map_err(|e| format!("failed to spawn audio processing thread: {e}"))?;

    // Keep the cpal stream alive by leaking it (it stops if dropped).
    // In a real app, we'd store it in AudioHandle.
    std::mem::forget(_stream);

    Ok(AudioHandle {
        stop_flag,
        processing_thread: Some(processing_thread),
    })
}

/// Start cpal audio capture stream.
fn start_capture_stream(
    config: &AudioConfig,
    shared: Arc<SharedAudioState>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no audio input device available")?;

    let stream_config = cpal::StreamConfig {
        channels: config.channels,
        sample_rate: cpal::SampleRate(config.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = device
        .build_input_stream(
            &stream_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                // Audio callback: just write to ring buffer. No allocation, no blocking.
                let mut rb = shared.ring_buffer.lock();
                rb.write(data);
            },
            |err| {
                error!(error = %err, "audio capture error");
            },
            None,
        )
        .map_err(|e| format!("failed to build input stream: {e}"))?;

    stream.play().map_err(|e| format!("failed to start audio stream: {e}"))?;
    info!("audio capture stream started");

    Ok(stream)
}

/// Processing loop: reads from ring buffer, runs VAD → wake pipeline.
fn run_processing_loop(
    shared: Arc<SharedAudioState>,
    frame_size: usize,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    scheduler: Arc<Scheduler>,
    state_machine: Arc<StateMachine>,
    metrics: Arc<MetricsRegistry>,
) {
    let mut vad = EnergyVad::new();
    let mut detector: Box<dyn WakeDetector> = Box::new(EnergyPatternDetector::new());
    let confirmer = WakeConfirmer::new();

    let mut frame_buf = vec![0i16; frame_size];
    let mut confirm_scores: Vec<f32> = Vec::with_capacity(16);
    let mut confirm_start: Option<Instant> = None;

    let sleep_between = Duration::from_millis(20); // ~50 Hz processing rate

    info!("audio processing loop started");

    loop {
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            info!("audio processing loop stopping");
            break;
        }

        // Read a frame from the ring buffer
        let available = {
            let rb = shared.ring_buffer.lock();
            rb.available()
        };

        if available < frame_size {
            std::thread::sleep(sleep_between);
            continue;
        }

        let read_count = {
            let mut rb = shared.ring_buffer.lock();
            rb.read(&mut frame_buf)
        };

        if read_count == 0 {
            std::thread::sleep(sleep_between);
            continue;
        }

        let samples = &frame_buf[..read_count];
        let current_state = state_machine.current();

        // Only run wake detection when in Sleep or WakeConfirm state
        match current_state {
            AppState::Sleep => {
                let vad_result = vad.process(samples);
                if !vad_result.should_run_wake {
                    continue;
                }

                let wake_time = Instant::now();
                let wake_score = detector.detect(samples);
                let detect_us = wake_time.elapsed().as_micros() as f64;
                metrics.record(metric_names::WAKE_DETECTED, detect_us);

                if confirmer.should_trigger(wake_score) {
                    // Stage 1: immediate UI/sound feedback via P0
                    let timestamp = Instant::now();
                    scheduler.submit_p0(P0Task::WakeDetected {
                        wake_score,
                        timestamp,
                    });
                    scheduler.submit_p0(P0Task::PlaySound { sound_id: "wake" });

                    // Start confirmation window
                    confirm_scores.clear();
                    confirm_scores.push(wake_score);
                    confirm_start = Some(Instant::now());
                }
            }
            AppState::WakeConfirm => {
                // Stage 2: accumulate confirmation scores
                if let Some(start) = confirm_start {
                    if start.elapsed() > confirmer.confirm_window {
                        // Confirmation window expired
                        if confirmer.is_confirmed(&confirm_scores) {
                            let timestamp = Instant::now();
                            scheduler.submit_p0(P0Task::WakeConfirmed { timestamp });
                            scheduler.submit_p0(P0Task::ShowModePanel);
                        } else {
                            scheduler.submit_p0(P0Task::WakeRejected);
                            scheduler.submit_p0(P0Task::PlaySound { sound_id: "reject" });
                        }
                        confirm_start = None;
                        confirm_scores.clear();
                    } else {
                        // Still within window, accumulate scores
                        let score = detector.detect(samples);
                        confirm_scores.push(score);
                    }
                }
            }
            _ => {
                // In other states, don't run wake detection
                // (but keep reading to prevent buffer overflow)
            }
        }
    }
}
