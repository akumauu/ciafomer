//! Observability: per-request tracing IDs, histogram metrics, timing spans.
//! Every request carries trace_id, request_id, generation.
//! Histograms track p50/p95/p99 for all timing points.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Identifiers attached to every request flowing through the system.
#[derive(Debug, Clone)]
pub struct RequestIds {
    pub trace_id: String,
    pub request_id: String,
    pub generation: u64,
}

impl RequestIds {
    pub fn new(generation: u64) -> Self {
        Self {
            trace_id: uuid::Uuid::new_v4().to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            generation,
        }
    }
}

/// A span measuring elapsed time from creation to explicit end.
pub struct TimingSpan {
    name: &'static str,
    start: Instant,
    registry: Arc<MetricsRegistry>,
}

impl TimingSpan {
    pub fn new(name: &'static str, registry: Arc<MetricsRegistry>) -> Self {
        Self {
            name,
            start: Instant::now(),
            registry,
        }
    }

    /// End the span, recording elapsed duration in microseconds.
    pub fn finish(self) -> f64 {
        let elapsed_us = self.start.elapsed().as_micros() as f64;
        self.registry.record(self.name, elapsed_us);
        elapsed_us
    }

    /// Elapsed so far without finishing.
    pub fn elapsed_us(&self) -> f64 {
        self.start.elapsed().as_micros() as f64
    }
}

/// Fixed-capacity ring buffer for histogram samples.
struct SampleRing {
    samples: Vec<f64>,
    pos: usize,
    count: usize,
    capacity: usize,
}

impl SampleRing {
    fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0.0; capacity],
            pos: 0,
            count: 0,
            capacity,
        }
    }

    fn push(&mut self, value: f64) {
        self.samples[self.pos] = value;
        self.pos = (self.pos + 1) % self.capacity;
        if self.count < self.capacity {
            self.count += 1;
        }
    }

    fn percentile(&self, p: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.samples[..self.count].to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((p / 100.0) * (self.count as f64 - 1.0)).round() as usize;
        let idx = idx.min(self.count - 1);
        sorted[idx]
    }
}

/// Stores histograms for all named metrics.
pub struct MetricsRegistry {
    histograms: Mutex<HashMap<&'static str, SampleRing>>,
    ring_capacity: usize,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            histograms: Mutex::new(HashMap::new()),
            ring_capacity: 1024,
        }
    }

    /// Record a sample (in microseconds) for the named metric.
    pub fn record(&self, name: &'static str, value_us: f64) {
        let mut hists = self.histograms.lock();
        hists
            .entry(name)
            .or_insert_with(|| SampleRing::new(self.ring_capacity))
            .push(value_us);
        tracing::debug!(metric = name, value_us = value_us, "metric_recorded");
    }

    /// Start a timing span that records on finish.
    pub fn span(self: &Arc<Self>, name: &'static str) -> TimingSpan {
        TimingSpan::new(name, Arc::clone(self))
    }

    /// Get percentile for a metric (p value 0-100). Returns microseconds.
    pub fn percentile(&self, name: &str, p: f64) -> f64 {
        let hists = self.histograms.lock();
        hists
            .get(name)
            .map(|ring| ring.percentile(p))
            .unwrap_or(0.0)
    }

    /// Generate a summary of all metrics at p50/p95/p99.
    pub fn summary(&self) -> HashMap<String, MetricSummary> {
        let hists = self.histograms.lock();
        let mut out = HashMap::new();
        for (&name, ring) in hists.iter() {
            out.insert(
                name.to_string(),
                MetricSummary {
                    p50_us: ring.percentile(50.0),
                    p95_us: ring.percentile(95.0),
                    p99_us: ring.percentile(99.0),
                    count: ring.count,
                },
            );
        }
        out
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricSummary {
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub count: usize,
}

/// Well-known metric names (constants to avoid typos).
pub mod metric_names {
    pub const WAKE_DETECTED: &str = "t_wake_detected";
    pub const WAKE_UI_EMITTED: &str = "t_wake_ui_emitted";
    pub const MODE_PANEL_VISIBLE: &str = "t_mode_panel_visible";
    pub const CAPTURE_DONE: &str = "t_capture_done";
    pub const OCR_DONE: &str = "t_ocr_done";
    pub const TRANSLATE_FIRST_CHUNK: &str = "t_translate_first_chunk";
    pub const TRANSLATE_DONE: &str = "t_translate_done";
    pub const RENDER_DONE: &str = "t_render_done";
    pub const QUEUE_WAIT_P0: &str = "queue_wait_p0";
    pub const QUEUE_WAIT_P1: &str = "queue_wait_p1";
    pub const QUEUE_WAIT_P2: &str = "queue_wait_p2";
    pub const CANCEL_LATENCY: &str = "cancel_latency";
    // Phase 4: Realtime incremental
    pub const REALTIME_CYCLE: &str = "t_realtime_cycle";
}
