use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Canonical stream format for every producer and consumer of the bridge
/// (librespot sink, YouTube/file feeder, DJ overlay, Songbird reader). The
/// other modules alias these into their local integer types.
pub const SAMPLE_RATE: usize = 44_100;
pub const CHANNELS: usize = 2;
/// A complete overlay is held separately from the live music ring. Short
/// effects and DJ speech must not lose their tail when that ring is smaller.
pub const MAX_OVERLAY_SAMPLES: usize = SAMPLE_RATE * CHANNELS * 30;
const OVERLAY_MUSIC_GAIN: f32 = 0.35;
const DUCK_ATTACK_STEP: f32 = (1.0 - OVERLAY_MUSIC_GAIN) / (SAMPLE_RATE as f32 * 0.010);
const DUCK_RELEASE_STEP: f32 = (1.0 - OVERLAY_MUSIC_GAIN) / (SAMPLE_RATE as f32 * 0.120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OverlayStatus {
    Playing,
    Drained,
    Cancelled,
}

/// Identity and completion belong to one clip, never whichever clip happens
/// to occupy the lane when its caller finishes cleaning up.
#[derive(Clone, Debug)]
pub struct OverlayHandle {
    id: u64,
    status: Arc<AtomicU8>,
}

impl OverlayHandle {
    pub fn status(&self) -> OverlayStatus {
        match self.status.load(Ordering::Acquire) {
            0 => OverlayStatus::Playing,
            1 => OverlayStatus::Drained,
            _ => OverlayStatus::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayError {
    Stale,
    Busy,
    InvalidSamples,
    InvalidGain,
    TooLong,
}

struct OverlayClip {
    samples: Vec<f32>,
    cursor: usize,
    gain: f32,
    handle: OverlayHandle,
}

struct OverlayLane {
    epoch: u64,
    next_id: u64,
    clip: Option<OverlayClip>,
    music_gain: f32,
}

impl Default for OverlayLane {
    fn default() -> Self {
        Self {
            epoch: 0,
            next_id: 0,
            clip: None,
            music_gain: 1.0,
        }
    }
}

impl OverlayLane {
    fn remaining(&self) -> usize {
        self.clip
            .as_ref()
            .map_or(0, |clip| clip.samples.len() - clip.cursor)
    }
}

/// Wall-clock playout deadline after `frames_sent` frames from `start`. Both
/// real-time producers (librespot sink, YouTube/file feeder) pace against
/// this one computation so their deadline math can't drift apart.
pub fn playout_deadline(start: std::time::Instant, frames_sent: u64) -> std::time::Instant {
    start + std::time::Duration::from_secs_f64(frames_sent as f64 / SAMPLE_RATE as f64)
}
fn max_samples(buffer_seconds: usize) -> usize {
    SAMPLE_RATE * CHANNELS * buffer_seconds
}

/// How much audio survives an overflow, in seconds. Hitting the cap means
/// the consumer stalled; for a live stream the samples that just arrived
/// are the ones worth keeping, so the stale front is dropped down to this
/// cushion. Sized to sit inside the healthy steady-state fill (~0.3s).
const OVERFLOW_KEEP_SECONDS: f32 = 0.5;

fn overflow_keep_samples() -> usize {
    (((SAMPLE_RATE * CHANNELS) as f32 * OVERFLOW_KEEP_SECONDS) as usize) & !1
}

/// Shared audio buffer between the producers — librespot's sink, the
/// YouTube/file feeder, and a bounded soundboard/DJ overlay lane —
/// and the Discord consumer (SimpleBridgeReader).
/// Holds 44.1kHz stereo f32 samples; Songbird handles resampling to 48kHz.
pub struct AudioBridge {
    buffer: Mutex<VecDeque<f32>>,
    overlay: Mutex<OverlayLane>,
    /// Pausing music preserves its ring contents while the shared consumer
    /// remains running for effects and speech.
    music_paused: std::sync::atomic::AtomicBool,
    max_samples: usize,
    stats: BridgeStats,
    /// Set by the player actor while a media item holds the turn: the
    /// librespot sink drops its samples instead of pushing them, so a
    /// phone-side play press can't bleed Spotify audio over the item
    /// before the actor's `Pause` lands.
    spotify_muted: std::sync::atomic::AtomicBool,
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
            overlay: Mutex::new(OverlayLane::default()),
            music_paused: std::sync::atomic::AtomicBool::new(false),
            max_samples: cap,
            stats: BridgeStats::default(),
            spotify_muted: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Gate the librespot sink: `true` drops Spotify samples at the sink.
    pub fn set_spotify_muted(&self, muted: bool) {
        self.spotify_muted
            .store(muted, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn spotify_muted(&self) -> bool {
        self.spotify_muted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Push music samples (44.1kHz stereo f32). Called by the librespot sink
    /// and the YouTube/file feeder; effects and speech use start_overlay.
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
        // Keep every drop on whole stereo frames (even counts) so the L/R
        // interleaving in the buffer never shifts by one sample.
        let to_take = (samples.len().min(self.max_samples)) & !1;
        if to_take < samples.len() {
            // One push larger than the entire buffer: the tail cannot fit
            // however the front is managed.
            self.stats.total_dropped.fetch_add(
                (samples.len() - to_take) as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        // Overflow means the consumer stalled. Dropping what just arrived
        // would leave the buffer pinned at capacity, so playback would run
        // permanently behind live and never recover; drop the stale front
        // instead, costing one audible jump and restoring normal latency.
        let overflow = (buffer.len() + to_take).saturating_sub(self.max_samples);
        if overflow > 0 {
            let keep = overflow_keep_samples().min(self.max_samples.saturating_sub(to_take));
            let discard = buffer.len().saturating_sub(keep) & !1;
            buffer.drain(..discard);
            self.stats
                .total_dropped
                .fetch_add(discard as u64, std::sync::atomic::Ordering::Relaxed);
            if count.is_multiple_of(200) {
                tracing::warn!(
                    target: "audio_stream",
                    dropped = discard,
                    total_dropped = self.stats.total_dropped.load(std::sync::atomic::Ordering::Relaxed),
                    "bridge full, dropped the backlog to catch up to live"
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

    /// Called by Songbird to pull audio samples (44.1kHz stereo f32).
    /// Returns the sample span supplied by either lane. Unfilled output is
    /// silence, including during a music pause; the consumer never treats an
    /// overlay as starvation just because no music arrived.
    pub fn pull_samples(&self, output: &mut [f32]) -> usize {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut buffer = self.buffer.lock();
        // Drain whole stereo frames only, so the buffer's read position stays
        // frame-aligned and L/R can't swap on a later pull.
        let available = if self.music_paused.load(Ordering::Relaxed) {
            0
        } else {
            (buffer.len().min(output.len())) & !1
        };
        output.fill(0.0);

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

        // Ease music down under the clip and back up after it, using the
        // same gain for both channels. Clamp only mixed samples: ordinary
        // music keeps its original path and sample values after release.
        let mut overlay = self.overlay.lock();
        let overlay_available = overlay.remaining().min(output.len()) & !1;
        let mut music_gain = overlay.music_gain;
        let clip_gain = overlay.clip.as_ref().map_or(0.0, |clip| clip.gain);
        for (frame_index, frame) in output.chunks_exact_mut(CHANNELS).enumerate() {
            let offset = frame_index * CHANNELS;
            let mixing = offset < overlay_available;
            music_gain = if mixing && clip_gain > 0.0 {
                (music_gain - DUCK_ATTACK_STEP).max(OVERLAY_MUSIC_GAIN)
            } else {
                (music_gain + DUCK_RELEASE_STEP).min(1.0)
            };
            if mixing && clip_gain > 0.0 {
                let clip = overlay.clip.as_ref().expect("available overlay has a clip");
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let effect = clip.samples[clip.cursor + offset + channel];
                    *sample = (*sample * music_gain + effect * clip_gain).clamp(-1.0, 1.0);
                }
            } else if music_gain != 1.0 {
                for sample in frame {
                    *sample *= music_gain;
                }
            } else {
                // Nothing more can affect this block once the clip ended
                // and its release envelope has returned to unity.
                break;
            }
        }
        overlay.music_gain = music_gain;
        if let Some(clip) = overlay.clip.as_mut() {
            clip.cursor += overlay_available;
            if overlay_available > 0 && clip.cursor == clip.samples.len() {
                clip.handle
                    .status
                    .store(OverlayStatus::Drained as u8, Ordering::Release);
            }
            // Retain completed storage: destruction happens on control-side
            // start/cancel/clear, never on the audio callback.
        }

        available.max(overlay_available)
    }

    /// Capture before asynchronous decode or synthesis. A music reset makes
    /// results prepared for the previous session ineligible for playback.
    pub fn overlay_epoch(&self) -> u64 {
        self.overlay.lock().epoch
    }

    /// Admit one complete clip at its own gain. Occupied lanes reject another
    /// clip instead of mixing voices together or silently building a backlog.
    /// Input validation and allocation happen on this control path only.
    pub fn start_overlay(
        &self,
        expected_epoch: u64,
        samples: Vec<f32>,
        gain: f32,
    ) -> Result<OverlayHandle, OverlayError> {
        if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
            return Err(OverlayError::InvalidGain);
        }
        if samples.len() > MAX_OVERLAY_SAMPLES {
            return Err(OverlayError::TooLong);
        }
        if samples.is_empty()
            || !samples.len().is_multiple_of(CHANNELS)
            || samples.iter().any(|sample| !sample.is_finite())
        {
            return Err(OverlayError::InvalidSamples);
        }
        let mut overlay = self.overlay.lock();
        if overlay.epoch != expected_epoch {
            return Err(OverlayError::Stale);
        }
        if overlay.remaining() != 0 {
            return Err(OverlayError::Busy);
        }
        overlay.next_id = overlay.next_id.wrapping_add(1);
        let handle = OverlayHandle {
            id: overlay.next_id,
            status: Arc::new(AtomicU8::new(OverlayStatus::Playing as u8)),
        };
        overlay.clip = Some(OverlayClip {
            samples,
            cursor: 0,
            gain,
            handle: handle.clone(),
        });
        Ok(handle)
    }

    /// A late cleanup cannot remove a replacement clip. Drained clips keep
    /// their successful terminal status when their retained storage is freed.
    pub fn cancel_overlay(&self, handle: &OverlayHandle) {
        let mut overlay = self.overlay.lock();
        if overlay.clip.as_ref().is_some_and(|clip| {
            clip.handle.id == handle.id && Arc::ptr_eq(&clip.handle.status, &handle.status)
        }) {
            if let Some(clip) = overlay.clip.take() {
                if clip.handle.status() == OverlayStatus::Playing {
                    clip.handle
                        .status
                        .store(OverlayStatus::Cancelled as u8, Ordering::Release);
                }
            }
        }
    }

    pub fn set_music_paused(&self, paused: bool) {
        self.music_paused.store(paused, Ordering::Relaxed);
    }

    /// Available frames from either lane, for the consumer's first-read
    /// prebuffer. A paused song's retained samples must not satisfy it.
    pub fn buffered_audio_len(&self) -> usize {
        let music = if self.music_paused.load(Ordering::Relaxed) {
            0
        } else {
            self.len()
        };
        music.max(self.overlay.lock().remaining())
    }

    /// Complete clips need no streaming prebuffer, even when shorter than
    /// the configured music prebuffer threshold.
    pub fn has_overlay_audio(&self) -> bool {
        self.overlay.lock().remaining() != 0
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

    /// Discard stale music at a transport boundary. An effect already in
    /// the room keeps playing, but pending synthesis from before the boundary
    /// cannot arrive afterwards. Music pause remains owned by the actor.
    pub fn clear_music(&self) {
        self.buffer.lock().clear();
        let mut overlay = self.overlay.lock();
        overlay.epoch = overlay.epoch.wrapping_add(1);
    }

    /// Stop every lane on a voice/session teardown. The next music owner
    /// explicitly releases its pause gate when playback starts again.
    pub fn clear(&self) {
        self.buffer.lock().clear();
        let mut overlay = self.overlay.lock();
        overlay.epoch = overlay.epoch.wrapping_add(1);
        overlay.music_gain = 1.0;
        if let Some(clip) = overlay.clip.take() {
            if clip.handle.status() == OverlayStatus::Playing {
                clip.handle
                    .status
                    .store(OverlayStatus::Cancelled as u8, Ordering::Release);
            }
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
        assert_eq!(
            b.pull_samples(&mut out),
            2,
            "odd request drains an even count"
        );
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
            let chunk: Vec<f32> = (0..100)
                .map(|_| {
                    let v = next;
                    next += 1.0;
                    v
                })
                .collect();
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
    fn a_stalled_consumer_keeps_the_newest_audio_not_the_oldest() {
        // Seen live: the buffer filled to its 10s cap and stayed pinned
        // there, so Discord ran ~10s behind Spotify until the bot was
        // restarted. Refusing the new audio is what pinned it — the stale
        // front has to go instead, so playback catches back up to live.
        let b = AudioBridge::new(1); // cap = 88_200 samples = 1s
        for i in 0..100u32 {
            b.push_samples(&vec![i as f32; 2048]); // nothing ever pulls
        }
        let mut out = vec![0.0f32; b.len()];
        let n = b.pull_samples(&mut out);
        assert!(n > 0);
        assert_eq!(out[n - 1], 99.0, "the newest audio survives");
        assert!(
            out[0] > 0.0,
            "the oldest audio was discarded, not the newest"
        );
        assert!(b.stats_snapshot().total_dropped > 0);
    }

    #[test]
    fn catching_up_keeps_the_newest_audio_and_stays_frame_aligned() {
        let b = AudioBridge::new(1);
        b.push_samples(&vec![0.5f32; 88_200]); // fill to cap with stale audio
        b.push_samples(&[1.0, 2.0, 3.0, 4.0]); // fresh arrival
        let mut out = vec![0.0f32; b.len()];
        let n = b.pull_samples(&mut out);
        assert!(n.is_multiple_of(2), "drains whole stereo frames");
        assert_eq!(
            &out[n - 4..n],
            &[1.0, 2.0, 3.0, 4.0],
            "the newest samples survive the catch-up"
        );
    }

    #[test]
    fn clear_empties_the_buffer() {
        let b = AudioBridge::new(1);
        b.push_samples(&[1.0, 2.0]);
        b.clear();
        assert_eq!(b.len(), 0);
    }

    // --- Shared soundboard / DJ overlay lane ---

    fn start(b: &AudioBridge, samples: Vec<f32>, gain: f32) -> OverlayHandle {
        b.start_overlay(b.overlay_epoch(), samples, gain).unwrap()
    }

    #[test]
    fn overlay_plays_at_its_own_gain_without_music_and_zero_fills_the_tail() {
        for gain in [0.0, 0.18, 1.0] {
            let b = AudioBridge::new(1);
            let handle = start(&b, vec![0.5, -0.25], gain);
            let mut out = [99.0; 4];
            assert_eq!(b.pull_samples(&mut out), 2, "overlay is audible data");
            assert_eq!(out, [0.5 * gain, -0.25 * gain, 0.0, 0.0]);
            assert_eq!(handle.status(), OverlayStatus::Drained);
        }
    }

    #[test]
    fn music_pauses_without_consuming_its_frames_while_an_overlay_plays() {
        let b = AudioBridge::new(1);
        b.push_samples(&[0.25, -0.5, 0.5, -0.25]);
        b.set_music_paused(true);
        assert_eq!(b.buffered_audio_len(), 0);
        let handle = start(&b, vec![0.75, -0.75], 1.0);
        assert_eq!(b.buffered_audio_len(), 2);
        let mut out = [99.0; 4];
        assert_eq!(b.pull_samples(&mut out), 2);
        assert_eq!(out, [0.75, -0.75, 0.0, 0.0]);
        assert_eq!(b.len(), 4, "paused song's cursor did not move");
        assert_eq!(handle.status(), OverlayStatus::Drained);
        assert_eq!(b.pull_samples(&mut out), 0);
        assert_eq!(out, [0.0; 4]);
        // Let the release finish while paused, then resume the exact music.
        b.pull_samples(&mut vec![0.0; SAMPLE_RATE * CHANNELS]);
        b.set_music_paused(false);
        assert_eq!(b.pull_samples(&mut out), 4);
        assert_eq!(out, [0.25, -0.5, 0.5, -0.25]);
    }

    #[test]
    fn music_is_ducked_in_stereo_then_restored_without_clipping_the_mix() {
        let b = AudioBridge::new(1);
        let clip_samples = SAMPLE_RATE * CHANNELS / 20; // 50 ms
        b.push_samples(&vec![0.5; SAMPLE_RATE * CHANNELS / 2]);
        start(&b, vec![0.25; clip_samples], 1.0);
        let mut out = vec![0.0; clip_samples];
        b.pull_samples(&mut out);
        assert!(out[0] < 0.75 && out[0] > 0.74, "attack starts gradually");
        assert!(out.chunks_exact(2).all(|frame| frame[0] == frame[1]));
        assert!((out[clip_samples - 1] - (0.5 * OVERLAY_MUSIC_GAIN + 0.25)).abs() < 1e-6);
        let mut release = vec![0.0; SAMPLE_RATE * CHANNELS / 4];
        b.pull_samples(&mut release);
        assert!(release[0] > 0.5 * OVERLAY_MUSIC_GAIN && release[0] < 0.18);
        assert_eq!(
            *release.last().unwrap(),
            0.5,
            "music returns exactly to unity"
        );

        b.clear();
        b.push_samples(&[1.0, -1.0]);
        start(&b, vec![1.0, -1.0], 1.0);
        let mut peaks = [0.0; 2];
        b.pull_samples(&mut peaks);
        assert_eq!(peaks, [1.0, -1.0]);
    }

    #[test]
    fn muted_overlay_does_not_duck_the_music() {
        for music in [[0.25, -0.5], [2.0, -2.0]] {
            let b = AudioBridge::new(1);
            b.push_samples(&music);
            start(&b, vec![0.9, 0.9], 0.0);
            let mut out = [0.0; 2];
            b.pull_samples(&mut out);
            assert_eq!(out, music);
        }
    }

    #[test]
    fn overlay_drains_complete_frames_at_consumer_pace_without_interleaving() {
        let b = AudioBridge::new(1);
        let handle = start(&b, vec![0.1, 0.2, 0.3, 0.4], 1.0);
        assert_eq!(
            b.start_overlay(b.overlay_epoch(), vec![0.9, 0.9], 1.0)
                .unwrap_err(),
            OverlayError::Busy
        );
        let mut out = [99.0; 3];
        assert_eq!(b.pull_samples(&mut out), 2);
        assert_eq!(out, [0.1, 0.2, 0.0]);
        assert_eq!(handle.status(), OverlayStatus::Playing);
        assert_eq!(b.pull_samples(&mut out), 2);
        assert_eq!(out, [0.3, 0.4, 0.0]);
        assert_eq!(handle.status(), OverlayStatus::Drained);
        assert!(!b.has_overlay_audio());
    }

    #[test]
    fn a_clip_longer_than_the_music_ring_keeps_its_complete_tail() {
        let b = AudioBridge::new(1);
        let sample_count = SAMPLE_RATE * CHANNELS + 2;
        let mut clip = vec![0.5; sample_count];
        clip[sample_count - 2..].copy_from_slice(&[0.25, -0.75]);
        let handle = start(&b, clip, 1.0);
        let mut out = vec![0.0; sample_count - 2];
        assert_eq!(b.pull_samples(&mut out), sample_count - 2);
        assert!(out.iter().all(|&sample| sample == 0.5));
        assert_eq!(handle.status(), OverlayStatus::Playing);
        let mut tail = [0.0; 2];
        assert_eq!(b.pull_samples(&mut tail), 2);
        assert_eq!(tail, [0.25, -0.75]);
        assert_eq!(handle.status(), OverlayStatus::Drained);
    }

    #[test]
    fn late_cleanup_cannot_cancel_a_replacement_and_terminal_status_is_stable() {
        let b = AudioBridge::new(1);
        let old = start(&b, vec![0.1, 0.1], 1.0);
        b.pull_samples(&mut [0.0; 2]);
        let replacement = start(&b, vec![0.9, 0.9], 1.0);
        b.cancel_overlay(&old);
        assert_eq!(old.status(), OverlayStatus::Drained);
        assert_eq!(replacement.status(), OverlayStatus::Playing);
        b.cancel_overlay(&replacement);
        assert_eq!(replacement.status(), OverlayStatus::Cancelled);
        let latest = start(&b, vec![0.5, 0.5], 1.0);
        b.cancel_overlay(&replacement);
        assert_eq!(latest.status(), OverlayStatus::Playing);
        let mut out = [0.0; 2];
        assert_eq!(b.pull_samples(&mut out), 2);
        assert_eq!(out, [0.5, 0.5]);
        b.clear();
        assert_eq!(latest.status(), OverlayStatus::Drained);
    }

    #[test]
    fn music_clear_preserves_current_clip_and_pause_but_fences_delayed_synthesis() {
        let b = AudioBridge::new(1);
        b.push_samples(&[0.5, 0.5]);
        b.set_music_paused(true);
        let old_epoch = b.overlay_epoch();
        let handle = start(&b, vec![0.1, 0.2], 1.0);
        b.clear_music();
        assert_eq!(b.len(), 0);
        assert_eq!(handle.status(), OverlayStatus::Playing);
        assert_eq!(
            b.start_overlay(old_epoch, vec![0.9, 0.9], 1.0).unwrap_err(),
            OverlayError::Stale
        );
        let mut out = [0.0; 2];
        assert_eq!(b.pull_samples(&mut out), 2);
        assert_eq!(out, [0.1, 0.2]);
        b.push_samples(&[0.5, 0.5]);
        assert_eq!(b.buffered_audio_len(), 0, "music remains paused");
        assert_eq!(b.pull_samples(&mut out), 0);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn full_clear_cancels_clip_and_prevents_late_synthesis_resurrection() {
        let b = AudioBridge::new(1);
        let epoch = b.overlay_epoch();
        let handle = start(&b, vec![0.1, 0.2], 1.0);
        b.clear();
        assert_eq!(handle.status(), OverlayStatus::Cancelled);
        assert!(!b.has_overlay_audio());
        assert_eq!(
            b.start_overlay(epoch, vec![0.9, 0.9], 1.0).unwrap_err(),
            OverlayError::Stale
        );
        let replacement = start(&b, vec![0.25, 0.25], 1.0);
        b.cancel_overlay(&handle);
        assert_eq!(replacement.status(), OverlayStatus::Playing);
    }

    #[test]
    fn invalid_audio_and_gain_are_rejected_without_occupying_the_lane() {
        let b = AudioBridge::new(1);
        let epoch = b.overlay_epoch();
        for samples in [
            vec![],
            vec![0.5],
            vec![f32::NAN, 0.0],
            vec![0.0, f32::INFINITY],
        ] {
            assert_eq!(
                b.start_overlay(epoch, samples, 1.0).unwrap_err(),
                OverlayError::InvalidSamples
            );
        }
        for gain in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            assert_eq!(
                b.start_overlay(epoch, vec![0.5, 0.5], gain).unwrap_err(),
                OverlayError::InvalidGain
            );
        }
        assert_eq!(
            b.start_overlay(epoch, vec![0.0; MAX_OVERLAY_SAMPLES + 2], 1.0)
                .unwrap_err(),
            OverlayError::TooLong
        );
        assert!(!b.has_overlay_audio());
        assert!(b.start_overlay(epoch, vec![0.5, 0.5], 1.0).is_ok());
    }

    // --- Shared pacing deadline ---

    #[test]
    fn playout_deadline_is_frames_over_sample_rate() {
        let start = std::time::Instant::now();
        assert_eq!(playout_deadline(start, 0), start);
        let one_second = playout_deadline(start, SAMPLE_RATE as u64);
        assert_eq!(one_second - start, std::time::Duration::from_secs(1));
        // 250ms worth of frames — the fractional path.
        let quarter = playout_deadline(start, SAMPLE_RATE as u64 / 4);
        let d = quarter - start;
        assert!((d.as_secs_f64() - 0.25).abs() < 1e-6, "got {d:?}");
    }
}
