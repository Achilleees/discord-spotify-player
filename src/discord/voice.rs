use crate::audio_bridge::AudioBridge;
use serenity::async_trait;
use songbird::events::{Event, EventContext, EventHandler as SongbirdEventHandler};
use songbird::input::core::io::MediaSource;
use songbird::input::{Input, RawAdapter};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: u32 = 2;

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
/// The prebuffer mechanism blocks the initial read until a minimum number of
/// samples have accumulated, smoothing out start/unpause audio stutters.
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

        // On first read, wait until at least one sample arrives (max 5s).
        // This avoids pulling silence before librespot starts pushing, without
        // accumulating a large buffer that causes catchup speed issues.
        if !self.prebuffer_done {
            let start = std::time::Instant::now();
            while self.bridge.len() == 0
                && start.elapsed() < std::time::Duration::from_secs(5)
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

        // samples_needed = buf.len() / 4, so every write fits.
        let bytes_written = samples_needed * 4;
        for (chunk, &sample) in buf[..bytes_written]
            .chunks_exact_mut(4)
            .zip(self.scratch[..samples_needed].iter())
        {
            chunk.copy_from_slice(&sample.to_le_bytes());
        }

        // Pace the stream so Songbird cannot drain the source faster than real-time.
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
