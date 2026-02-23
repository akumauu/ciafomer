//! Audio ring buffer: fixed pre-allocated circular buffer for PCM samples.
//! Capacity: 3 seconds at configured sample rate. No dynamic allocation.

/// Fixed-size ring buffer for PCM i16 samples. Pre-allocated, never grows.
pub struct RingBuffer {
    buffer: Box<[i16]>,
    write_pos: usize,
    read_pos: usize,
    capacity: usize,
    sample_rate: u32,
}

impl RingBuffer {
    /// Create a ring buffer sized for `duration_secs` at `sample_rate` Hz, mono.
    pub fn new(sample_rate: u32, duration_secs: f32) -> Self {
        let capacity = (sample_rate as f32 * duration_secs) as usize;
        Self {
            buffer: vec![0i16; capacity].into_boxed_slice(),
            write_pos: 0,
            read_pos: 0,
            capacity,
            sample_rate,
        }
    }

    /// Write samples into the ring buffer. Overwrites oldest data if full.
    /// Called from audio callback — must be lock-free and non-allocating.
    #[inline]
    pub fn write(&mut self, samples: &[i16]) {
        for &s in samples {
            self.buffer[self.write_pos] = s;
            self.write_pos = (self.write_pos + 1) % self.capacity;
        }
    }

    /// Read available samples into output buffer.
    /// Returns the number of samples actually read.
    #[inline]
    pub fn read(&mut self, output: &mut [i16]) -> usize {
        let available = self.available();
        let to_read = output.len().min(available);
        for i in 0..to_read {
            output[i] = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % self.capacity;
        }
        to_read
    }

    /// Number of unread samples available.
    #[inline]
    pub fn available(&self) -> usize {
        if self.write_pos >= self.read_pos {
            self.write_pos - self.read_pos
        } else {
            self.capacity - self.read_pos + self.write_pos
        }
    }

    /// Read the last N samples without advancing read pointer.
    /// Returns a newly allocated Vec (used infrequently, e.g., for wake analysis).
    pub fn peek_last(&self, n: usize) -> Vec<i16> {
        let n = n.min(self.capacity);
        let mut out = vec![0i16; n];
        let start = if self.write_pos >= n {
            self.write_pos - n
        } else {
            self.capacity - (n - self.write_pos)
        };
        for i in 0..n {
            out[i] = self.buffer[(start + i) % self.capacity];
        }
        out
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Reset read position to catch up to write position (discard unread data).
    pub fn reset_read(&mut self) {
        self.read_pos = self.write_pos;
    }
}
