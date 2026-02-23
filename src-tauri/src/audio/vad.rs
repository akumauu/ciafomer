//! Voice Activity Detection (VAD): energy-based with RMS gating.
//! Cascade: RMS energy gate → VAD decision.
//! When VAD is continuously false, wake inference frequency is reduced (1/4 rate).

/// RMS energy computation over a frame of PCM samples.
#[inline]
pub fn compute_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| {
        let f = s as f64;
        f * f
    }).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// Simple energy-based VAD.
pub struct EnergyVad {
    /// RMS threshold below which audio is considered silence.
    silence_threshold: f32,
    /// Number of consecutive silent frames before declaring no-voice.
    silence_frames_needed: u32,
    /// Current count of consecutive silent frames.
    silent_count: u32,
    /// Whether voice is currently detected.
    voice_active: bool,
    /// Frame counter for reduced wake inference rate.
    frame_counter: u64,
}

impl EnergyVad {
    pub fn new() -> Self {
        Self {
            silence_threshold: 300.0, // raw i16 RMS, ~= -40dB for 16-bit
            silence_frames_needed: 8,
            silent_count: 0,
            voice_active: false,
            frame_counter: 0,
        }
    }

    /// Process a frame of samples. Returns VAD decision.
    #[inline]
    pub fn process(&mut self, samples: &[i16]) -> VadResult {
        let rms = compute_rms(samples);
        self.frame_counter += 1;

        if rms < self.silence_threshold {
            self.silent_count += 1;
            if self.silent_count >= self.silence_frames_needed {
                self.voice_active = false;
            }
            VadResult {
                voice_active: self.voice_active,
                rms,
                should_run_wake: self.should_run_wake_inference(),
            }
        } else {
            self.silent_count = 0;
            self.voice_active = true;
            VadResult {
                voice_active: true,
                rms,
                should_run_wake: true,
            }
        }
    }

    /// When VAD is continuously false, only run wake inference every 4th frame.
    #[inline]
    fn should_run_wake_inference(&self) -> bool {
        if self.voice_active {
            true
        } else {
            // 1/4 rate when no voice activity
            self.frame_counter % 4 == 0
        }
    }

    pub fn is_voice_active(&self) -> bool {
        self.voice_active
    }
}

/// Result of VAD processing for one frame.
#[derive(Debug, Clone)]
pub struct VadResult {
    /// Whether voice is currently detected.
    pub voice_active: bool,
    /// RMS energy of the frame.
    pub rms: f32,
    /// Whether wake inference should run this frame (reduced rate when silent).
    pub should_run_wake: bool,
}
