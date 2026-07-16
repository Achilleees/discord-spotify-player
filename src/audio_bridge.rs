use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

const SPOTIFY_SAMPLE_RATE: usize = 44_100;
const CHANNELS: usize = 2;
/// Gain applied to DJ overlay samples when mixed on top of the music.
const OVERLAY_GAIN: f32 = 0.18;
fn max_samples(buffer_seconds: usize) -> usize {
    SPOTIFY_SAMPLE_RATE * CHANNELS * buffer_seconds
}

/// Shared audio buffer between Spotify (producer) and Discord (consumer)
/// Keeps Spotify's native 44.1kHz f32 samples; Songbird handles resampling to 48kHz.
pub struct AudioBridge {
    buffer: Mutex<VecDeque<f32>>,
    overlay: Mutex<VecDeque<f32>>,
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
        // Keep drops on whole stereo frames (even counts) so the L/R
        // interleaving in the buffer never shifts by one sample.
        let to_take = (samples.len().min(available_space)) & !1;
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
        // Drain whole stereo frames only, so the buffer's read position stays
        // frame-aligned and L/R can't swap on a later pull.
        let available = (buffer.len().min(output.len())) & !1;

        if count.is_multiple_of(100) {
            tracing::debug!(
                target: "audio_stream",
                calls = count,
                buffered = buffer.len(),
                requested = output.len(),
                "pull_samples"
            );
        }

        // Copy out of the deque's two contiguous slices, then drain.
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

        // Mix DJ overlay clips on top of the music at a fixed level.
        {
            let mut overlay = self.overlay.lock();
            // Even (stereo-frame) count, matching the main-buffer drains, so an
            // odd-length mix can't leave the overlay mid-frame and swap L/R.
            let overlay_available = (overlay.len().min(output.len())) & !1;
            if overlay_available > 0 {
                let (head, tail) = overlay.as_slices();
                for i in 0..overlay_available {
                    let ov_sample = if i < head.len() { head[i] } else { tail[i - head.len()] };
                    output[i] += ov_sample * OVERLAY_GAIN;
                }
                overlay.drain(..overlay_available);
            }
        }

        available
    }

    /// Push DJ/overlay samples that mix on top of the music.
    pub fn push_overlay(&self, samples: &[f32]) {
        let mut overlay = self.overlay.lock();
        // Bound overlay growth like the main buffer, so a fast clip source
        // can't grow it without limit.
        let space = self.max_samples.saturating_sub(overlay.len());
        let to_take = (samples.len().min(space)) & !1;
        let dropped = samples.len() - to_take;
        if dropped > 0 {
            tracing::warn!(
                target: "audio_stream",
                dropped,
                dropped_s = dropped as f64 / (44100.0 * 2.0),
                "overlay clip truncated to bridge capacity (tail cut mid-clip)"
            );
        }
        overlay.extend(samples[..to_take].iter());
        tracing::debug!(
            target: "audio_stream",
            samples = to_take,
            duration_s = to_take as f64 / (44100.0 * 2.0),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pull_roundtrips_values() {
        let b = AudioBridge::new(1);
        b.push_samples(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0f32; 4];
        assert_eq!(b.pull_samples(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn pull_drains_only_whole_stereo_frames() {
        let b = AudioBridge::new(1);
        b.push_samples(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0f32; 3]; // odd request
        assert_eq!(b.pull_samples(&mut out), 2, "odd request drains an even count");
        assert_eq!(&out[..2], &[1.0, 2.0]);
        assert_eq!(b.len(), 2, "one whole frame left behind");
    }

    #[test]
    fn push_drops_odd_tail_to_stay_frame_aligned() {
        let b = AudioBridge::new(1);
        b.push_samples(&[1.0, 2.0, 3.0]); // odd
        assert_eq!(b.len(), 2, "odd tail dropped to preserve L/R alignment");
    }

    #[test]
    fn survives_ring_wraparound() {
        let b = AudioBridge::new(1);
        // Push then fully drain many times so the internal ring wraps and the
        // head/tail split-copy path in pull_samples is exercised.
        let mut expected = 0.0f32;
        let mut next = 0.0f32;
        for _ in 0..1000 {
            let chunk: Vec<f32> = (0..100).map(|_| { let v = next; next += 1.0; v }).collect();
            b.push_samples(&chunk);
            let mut out = [0.0f32; 100];
            let n = b.pull_samples(&mut out);
            for &v in &out[..n] {
                assert_eq!(v, expected);
                expected += 1.0;
            }
        }
    }

    #[test]
    fn drops_when_full_and_counts_them() {
        let b = AudioBridge::new(1); // cap = 44100 * 2 * 1
        b.push_samples(&vec![0.5f32; 100_000]);
        assert_eq!(b.len(), 88_200);
        assert!(b.stats_snapshot().total_dropped >= (100_000 - 88_200));
    }

    #[test]
    fn clear_empties_the_buffer() {
        let b = AudioBridge::new(1);
        b.push_samples(&[1.0, 2.0]);
        b.clear();
        assert_eq!(b.len(), 0);
    }
}
