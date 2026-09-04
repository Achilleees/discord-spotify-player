use crate::audio_bridge::AudioBridge;
use serenity::async_trait;
use songbird::events::{Event, EventContext, EventHandler as SongbirdEventHandler};
use songbird::input::core::io::MediaSource;
use songbird::input::{Input, RawAdapter};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

// Local typed aliases of the canonical bridge format.
pub const SAMPLE_RATE: u32 = crate::audio_bridge::SAMPLE_RATE as u32;
pub const CHANNELS: u32 = crate::audio_bridge::CHANNELS as u32;

pub struct TrackErrorHandler;

#[async_trait]
impl SongbirdEventHandler for TrackErrorHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::Track(track_list) => {
                for (state, _handle) in *track_list {
                    tracing::warn!(
                        target: "discord_spotify_player::discord",
                        playing = ?state.playing,
                        play_time = ?state.play_time,
                        "track event"
                    );
                }
            }
            _ => {
                tracing::debug!(target: "discord_spotify_player::discord", event = ?ctx, "songbird event");
            }
        }
        None
    }
}

/// Reads raw f32 PCM samples from the AudioBridge for Songbird consumption.
/// Wraps in a RawAdapter which adds the SbirdRaw header that Songbird expects.
///
/// On the first read, blocks until `prebuffer_samples` have accumulated (or
/// `prebuffer_wait` elapses), so playback starts on a filled buffer instead of
/// stuttering through the initial silence.
pub struct SimpleBridgeReader {
    bridge: Arc<AudioBridge>,
    pos: u64,
    scratch: Vec<f32>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    prebuffer_done: bool,
}

impl SimpleBridgeReader {
    pub fn new(
        bridge: Arc<AudioBridge>,
        prebuffer_samples: usize,
        prebuffer_wait: std::time::Duration,
    ) -> Self {
        Self {
            bridge,
            pos: 0,
            scratch: Vec::new(),
            prebuffer_samples,
            prebuffer_wait,
            prebuffer_done: false,
        }
    }

    pub fn into_input(self) -> Input {
        let raw_adapter = RawAdapter::new(self, SAMPLE_RATE, CHANNELS);
        raw_adapter.into()
    }
}

impl Read for SimpleBridgeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        static READ_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = READ_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if count < 10 || count.is_multiple_of(500) {
            tracing::debug!(
                target: "audio_stream",
                call = count,
                pos = self.pos,
                buf_size = buf.len(),
                "bridge reader read"
            );
        }

        // A buffer too small to hold one f32 sample can't carry audio; return
        // silence rather than Ok(0), which the Read contract treats as EOF.
        if buf.len() < 4 {
            buf.fill(0);
            self.pos += buf.len() as u64;
            return Ok(buf.len());
        }

        // First read: block until PREBUFFER_SECONDS' worth of samples have
        // accumulated (or prebuffer_wait elapses), so playback starts on a
        // filled buffer instead of stuttering through the opening silence.
        if !self.prebuffer_done {
            let start = std::time::Instant::now();
            while self.bridge.len() < self.prebuffer_samples
                && start.elapsed() < self.prebuffer_wait
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            self.prebuffer_done = true;
        }

        let samples_needed = buf.len() / 4;
        if self.scratch.len() < samples_needed {
            self.scratch.resize(samples_needed, 0.0);
        }
        self.scratch[..samples_needed].fill(0.0);

        let samples_read = self
            .bridge
            .pull_samples(&mut self.scratch[..samples_needed]);
        if count < 10 || count.is_multiple_of(500) {
            tracing::debug!(
                target: "audio_stream",
                samples_read,
                samples_needed,
                "bridge reader pulled"
            );
        }

        if samples_read == 0 && count.is_multiple_of(200) {
            tracing::debug!(
                target: "audio_stream",
                "bridge reader starved"
            );
        }

        let bytes_written = samples_needed * 4;
        debug_assert!(bytes_written <= buf.len());
        for (chunk, &sample) in buf[..bytes_written]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(self.scratch[..samples_needed].iter())
        {
            chunk.copy_from_slice(&sample.to_le_bytes());
        }

        // Starvation backoff only — real-time pacing lives with the producers
        // (DiscordSink::write / the feeder), never in this reader.
        if samples_read == 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        self.pos += bytes_written as u64;
        Ok(bytes_written)
    }
}

impl Seek for SimpleBridgeReader {
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        Ok(self.pos)
    }
}

impl MediaSource for SimpleBridgeReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::SimpleBridgeReader;
    use crate::audio_bridge::AudioBridge;
    use std::io::Read as _;
    use std::time::{Duration, Instant};

    fn reader(bridge: &std::sync::Arc<AudioBridge>) -> SimpleBridgeReader {
        SimpleBridgeReader::new(bridge.clone(), 0, Duration::ZERO)
    }

    #[test]
    fn starved_reader_returns_silence_never_eof() {
        // The soak-critical invariant: Ok(0) is EOF to Songbird and ends the
        // track permanently. An empty bridge must yield a full buffer of
        // silence instead.
        let bridge = AudioBridge::new(1);
        let mut r = reader(&bridge);
        let mut buf = [0xFFu8; 64];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(n, 64, "still a full write, not EOF");
        assert!(buf.iter().all(|&b| b == 0), "silence, not stale bytes");
    }

    #[test]
    fn buffer_smaller_than_one_sample_gets_silence() {
        let bridge = AudioBridge::new(1);
        let mut r = reader(&bridge);
        let mut buf = [0xFFu8; 3];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(n, 3, "sub-sample buffer is zero-filled, not Ok(0)");
        assert_eq!(buf, [0, 0, 0]);
    }

    #[test]
    fn samples_are_packed_little_endian() {
        let bridge = AudioBridge::new(1);
        bridge.push_samples(&[0.5, -0.25]);
        let mut r = reader(&bridge);
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 8);
        let a = f32::from_le_bytes(buf[0..4].try_into().unwrap());
        let b = f32::from_le_bytes(buf[4..8].try_into().unwrap());
        assert_eq!((a, b), (0.5, -0.25));
    }

    #[test]
    fn partial_starvation_zero_fills_the_tail() {
        let bridge = AudioBridge::new(1);
        bridge.push_samples(&[1.0, 1.0]);
        let mut r = reader(&bridge);
        let mut buf = [0xFFu8; 16];
        assert_eq!(r.read(&mut buf).unwrap(), 16);
        assert_eq!(f32::from_le_bytes(buf[0..4].try_into().unwrap()), 1.0);
        assert_eq!(f32::from_le_bytes(buf[4..8].try_into().unwrap()), 1.0);
        assert!(buf[8..].iter().all(|&b| b == 0), "unfilled samples are silence");
    }

    #[test]
    fn odd_buffer_returns_whole_samples_only() {
        let bridge = AudioBridge::new(1);
        bridge.push_samples(&[1.0, 1.0]);
        let mut r = reader(&bridge);
        let mut buf = [0u8; 6]; // 1.5 samples
        assert_eq!(r.read(&mut buf).unwrap(), 4, "trailing half-sample not written");
    }

    #[test]
    fn first_read_blocks_until_prebuffer_timeout() {
        let bridge = AudioBridge::new(1);
        let mut r = SimpleBridgeReader::new(bridge, 4, Duration::from_millis(150));
        let mut buf = [0u8; 16];
        let t = Instant::now();
        assert_eq!(r.read(&mut buf).unwrap(), 16);
        assert!(
            t.elapsed() >= Duration::from_millis(140),
            "first read must wait out the prebuffer window on an empty bridge"
        );
        // Later reads must not re-block: prebuffering is first-read-only.
        let t = Instant::now();
        assert_eq!(r.read(&mut buf).unwrap(), 16);
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn prebuffer_releases_early_once_filled() {
        let bridge = AudioBridge::new(1);
        bridge.push_samples(&[1.0, 1.0]);
        let mut r = SimpleBridgeReader::new(bridge, 2, Duration::from_secs(5));
        let mut buf = [0u8; 8];
        let t = Instant::now();
        assert_eq!(r.read(&mut buf).unwrap(), 8);
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "a filled bridge must not wait out the full window"
        );
    }
}
