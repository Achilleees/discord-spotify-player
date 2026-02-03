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
            max_samples: cap,
            stats: BridgeStats::default(),
        })
    }

    /// Called by librespot to push audio samples (44.1kHz stereo f32)
    pub fn push_samples(&self, samples: &[f32]) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count % 100 == 0 {
            tracing::debug!(
                target: "audio_stream",
                "push_samples called {} times, samples: {}",
                count,
                samples.len()
            );
        }

        let mut buffer = self.buffer.lock();
        let available_space = self.max_samples.saturating_sub(buffer.len());
        if available_space == 0 {
            self.stats
                .total_dropped
                .fetch_add(samples.len() as u64, std::sync::atomic::Ordering::Relaxed);
            if count % 200 == 0 {
                tracing::warn!(
                    "AudioBridge full; dropping {} samples (total dropped {})",
                    samples.len(),
                    self.stats
                        .total_dropped
                        .load(std::sync::atomic::Ordering::Relaxed)
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
            if count % 200 == 0 {
                tracing::warn!(
                    "AudioBridge nearly full; dropping {} samples (total dropped {})",
                    samples.len() - to_take,
                    self.stats
                        .total_dropped
                        .load(std::sync::atomic::Ordering::Relaxed)
                );
            }
        }
        buffer.extend(samples.iter().take(to_take).copied());
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

        if count % 100 == 0 {
            tracing::debug!(
                target: "audio_stream",
                "pull_samples called {} times, buffer: {}, requested: {}",
                count,
                buffer.len(),
                output.len()
            );
        }

        for out in output.iter_mut().take(available) {
            if let Some(sample) = buffer.pop_front() {
                *out = sample;
            } else {
                break;
            }
        }

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

        available
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
    }
}

impl Default for AudioBridge {
    fn default() -> Self {
        let cap = max_samples(4);
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(cap)),
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
