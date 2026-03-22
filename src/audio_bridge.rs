use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

const SPOTIFY_SAMPLE_RATE: usize = 44_100;
const CHANNELS: usize = 2;
fn max_samples(buffer_seconds: usize) -> usize {
    SPOTIFY_SAMPLE_RATE * CHANNELS * buffer_seconds
}

/// Shared audio buffer between Spotify (producer) and Discord (consumer)
/// Keeps Spotify's native 44.1kHz f32 samples; Songbird handles resampling to 48kHz.
pub struct AudioBridge {
    buffer: Mutex<VecDeque<f32>>,
    overlay: Mutex<VecDeque<f32>>,
    overlay_duck_volume: std::sync::atomic::AtomicU32,  // f32 bits, music volume during overlay
    max_samples: usize,
    stats: BridgeStats,
}

#[derive(Default)]
struct BridgeStats {
    last_push_ms: std::sync::atomic::AtomicU64,
    last_pull_ms: std::sync::atomic::AtomicU64,
    last_nonzero_pull_ms: std::sync::atomic::AtomicU64,
    total_pushed: std::sync::atomic::AtomicU64,
    total_pulled: std::sync::atomic::AtomicU64,
    total_dropped: std::sync::atomic::AtomicU64,
}

impl AudioBridge {
    pub fn new(buffer_seconds: usize) -> Arc<Self> {
        let cap = max_samples(buffer_seconds);
        Arc::new(Self {
            buffer: Mutex::new(VecDeque::with_capacity(cap)),
            overlay: Mutex::new(VecDeque::with_capacity(cap)),
            overlay_duck_volume: std::sync::atomic::AtomicU32::new(f32::to_bits(1.0)),
            max_samples: cap,
            stats: BridgeStats::default(),
        })
    }

    /// Called by librespot to push audio samples (44.1kHz stereo f32)
    pub fn push_samples(&self, samples: &[f32]) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count.is_multiple_of(100) {
            tracing::debug!(
                target: "audio_stream",
                calls = count,
                samples = samples.len(),
                "push_samples"
            );
        }

        let mut buffer = self.buffer.lock();
        let available_space = self.max_samples.saturating_sub(buffer.len());
        if available_space == 0 {
            self.stats
                .total_dropped
                .fetch_add(samples.len() as u64, std::sync::atomic::Ordering::Relaxed);
            if count.is_multiple_of(200) {
                tracing::warn!(
                    target: "audio_stream",
                    dropped = samples.len(),
                    total_dropped = self.stats.total_dropped.load(std::sync::atomic::Ordering::Relaxed),
                    "bridge full, dropping samples"
                );
            }
            return;
        }
        let to_take = samples.len().min(available_space);
        if to_take < samples.len() {
            self.stats.total_dropped.fetch_add(
                (samples.len() - to_take) as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            if count.is_multiple_of(200) {
                tracing::warn!(
                    target: "audio_stream",
                    dropped = samples.len() - to_take,
                    total_dropped = self.stats.total_dropped.load(std::sync::atomic::Ordering::Relaxed),
                    "bridge nearly full, dropping samples"
                );
            }
        }
        buffer.extend(&samples[..to_take]);
        if to_take > 0 {
            self.stats
                .total_pushed
                .fetch_add(to_take as u64, std::sync::atomic::Ordering::Relaxed);
            self.stats
                .last_push_ms
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Called by Songbird to pull audio samples (44.1kHz stereo f32)
    /// Returns the number of samples read
    pub fn pull_samples(&self, output: &mut [f32]) -> usize {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut buffer = self.buffer.lock();
        let available = buffer.len().min(output.len());

        if count.is_multiple_of(100) {
            tracing::debug!(
                target: "audio_stream",
                calls = count,
                buffered = buffer.len(),
                requested = output.len(),
                "pull_samples"
            );
        }

        // Copy via VecDeque's two contiguous slices, then drain.
        // Avoids per-element pop_front and temporary Vec allocation.
        {
            let (head, tail) = buffer.as_slices();
            if available <= head.len() {
                output[..available].copy_from_slice(&head[..available]);
            } else {
                let from_head = head.len();
                output[..from_head].copy_from_slice(head);
                output[from_head..available].copy_from_slice(&tail[..available - from_head]);
            }
        }
        buffer.drain(..available);

        if available > 0 {
            self.stats
                .total_pulled
                .fetch_add(available as u64, std::sync::atomic::Ordering::Relaxed);
            self.stats
                .last_pull_ms
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
            self.stats
                .last_nonzero_pull_ms
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
        }

        // Mix overlay (DJ clips) on top of music with volume ducking
        {
            let mut overlay = self.overlay.lock();
            let overlay_available = overlay.len().min(output.len());
            if overlay_available > 0 {
                // Duck music volume during overlay
                let duck_vol: f32 = 1.0;
                self.overlay_duck_volume.store(f32::to_bits(duck_vol), std::sync::atomic::Ordering::Relaxed);
                let (head, tail) = overlay.as_slices();
                for i in 0..overlay_available {
                    let ov_sample = if i < head.len() { head[i] } else { tail[i - head.len()] };
                    output[i] = output[i] * duck_vol + ov_sample * 0.18;
                }
                overlay.drain(..overlay_available);
                // If overlay just emptied, start fade back
                if overlay.is_empty() {
                    self.overlay_duck_volume.store(f32::to_bits(1.0), std::sync::atomic::Ordering::Relaxed);
                }
            } else {
                // Smooth fade back to full volume
                let current_vol = f32::from_bits(self.overlay_duck_volume.load(std::sync::atomic::Ordering::Relaxed));
                if current_vol < 0.99 {
                    let new_vol = (current_vol + 0.02).min(1.0);
                    self.overlay_duck_volume.store(f32::to_bits(new_vol), std::sync::atomic::Ordering::Relaxed);
                    for sample in output[..available].iter_mut() {
                        *sample *= new_vol;
                    }
                }
            }
        }

        available
    }

    /// Push DJ/overlay samples that mix on top of music with volume ducking
    pub fn push_overlay(&self, samples: &[f32]) {
        let mut overlay = self.overlay.lock();
        overlay.extend(samples.iter());
        tracing::debug!(
            target: "audio_stream",
            samples = samples.len(),
            duration_s = samples.len() as f64 / (44100.0 * 2.0),
            "overlay samples pushed"
        );
    }

    pub fn len(&self) -> usize {
        self.buffer.lock().len()
    }

    pub fn stats_snapshot(&self) -> BridgeStatsSnapshot {
        BridgeStatsSnapshot {
            last_push_ms: self
                .stats
                .last_push_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            last_pull_ms: self
                .stats
                .last_pull_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            last_nonzero_pull_ms: self
                .stats
                .last_nonzero_pull_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            total_pushed: self
                .stats
                .total_pushed
                .load(std::sync::atomic::Ordering::Relaxed),
            total_pulled: self
                .stats
                .total_pulled
                .load(std::sync::atomic::Ordering::Relaxed),
            total_dropped: self
                .stats
                .total_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Clear the buffer (e.g., on pause/stop)
    pub fn clear(&self) {
        self.buffer.lock().clear();
        self.overlay.lock().clear();
        self.overlay_duck_volume.store(f32::to_bits(1.0), std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for AudioBridge {
    fn default() -> Self {
        let cap = max_samples(4);
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(cap)),
            overlay: Mutex::new(VecDeque::with_capacity(cap)),
            overlay_duck_volume: std::sync::atomic::AtomicU32::new(f32::to_bits(1.0)),
            max_samples: cap,
            stats: BridgeStats::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BridgeStatsSnapshot {
    pub last_push_ms: u64,
    pub last_pull_ms: u64,
    pub last_nonzero_pull_ms: u64,
    pub total_pushed: u64,
    pub total_pulled: u64,
    pub total_dropped: u64,
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
