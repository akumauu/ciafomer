//! Wake word detection module.
//! Pipeline: audio frames → RMS gate → VAD → wake inference.
//! Wake inference is a placeholder that can be replaced with a real model
//! (e.g., a small CNN or keyword-spotting model).
//! On detection: score is emitted; two-stage confirmation is handled by the caller.

/// Wake word detector trait (platform/model adapter).
pub trait WakeDetector: Send + Sync {
    /// Run wake detection on a frame of PCM i16 samples.
    /// Returns a wake score in [0.0, 1.0]. Higher = more confident.
    fn detect(&mut self, samples: &[i16]) -> f32;

    /// Reset internal state (e.g., between sessions).
    fn reset(&mut self);
}

/// Placeholder wake detector that uses simple energy-pattern matching.
/// In production, replace with a real keyword-spotting model.
pub struct EnergyPatternDetector {
    /// Smoothed energy tracking for pattern detection.
    prev_energy: f32,
    /// Spike detection: a sudden rise in energy may indicate wake word.
    spike_ratio_threshold: f32,
}

impl EnergyPatternDetector {
    pub fn new() -> Self {
        Self {
            prev_energy: 0.0,
            spike_ratio_threshold: 3.0,
        }
    }
}

impl WakeDetector for EnergyPatternDetector {
    fn detect(&mut self, samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let rms = super::vad::compute_rms(samples);
        let score = if self.prev_energy > 100.0 && rms > self.prev_energy * self.spike_ratio_threshold {
            // Energy spike detected — possible wake word
            let ratio = rms / self.prev_energy;
            // Normalize to [0, 1] with sigmoid-like curve
            let raw = (ratio - self.spike_ratio_threshold) / self.spike_ratio_threshold;
            raw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Exponential moving average for energy tracking
        self.prev_energy = self.prev_energy * 0.9 + rms * 0.1;
        score
    }

    fn reset(&mut self) {
        self.prev_energy = 0.0;
    }
}
