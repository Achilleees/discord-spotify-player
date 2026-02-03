use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::presence::PresenceUpdate;
use serenity::all::{ChannelId, GatewayIntents, GuildId, Ready};
use serenity::async_trait;
use serenity::client::{Client, Context, EventHandler};
use serenity::gateway::ActivityData;
use serenity::model::user::OnlineStatus;
use songbird::events::{Event, EventContext, EventHandler as SongbirdEventHandler, TrackEvent};
use songbird::input::RawAdapter;
use songbird::input::core::io::MediaSource;
use songbird::input::Input;
use songbird::SerenityInit;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use std::time::Duration;

/// Event handler for track state changes
struct TrackErrorHandler;

#[async_trait]
impl SongbirdEventHandler for TrackErrorHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::Track(track_list) => {
                for (state, _handle) in *track_list {
                    tracing::warn!("Track event - playing: {:?}, play_time: {:?}", state.playing, state.play_time);
                }
            }
            _ => {
                tracing::debug!("Songbird event: {:?}", ctx);
            }
        }
        None
    }
}

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u32 = 2;

/// Simple reader that outputs raw f32 PCM samples from the audio bridge
/// This will be wrapped by Songbird's RawAdapter which adds the SbirdRaw header
struct SimpleBridgeReader {
    bridge: Arc<AudioBridge>,
    pos: u64,
    scratch: Vec<f32>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    prebuffer_done: bool,
}

impl SimpleBridgeReader {
    fn new(
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
}

impl Read for SimpleBridgeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        static READ_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = READ_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if count < 10 || count % 500 == 0 {
            tracing::debug!(
                target: "audio_stream",
                "SimpleBridgeReader: read #{}, pos={}, buf_size={}",
                count,
                self.pos,
                buf.len()
            );
        }

        // Wait for a minimum buffer fill to smooth out start/pause stutters.
        if !self.prebuffer_done
            && self.prebuffer_samples > 0
            && self.bridge.len() < self.prebuffer_samples
        {
            let start = std::time::Instant::now();
            while self.bridge.len() < self.prebuffer_samples && start.elapsed() < self.prebuffer_wait {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            self.prebuffer_done = true;
        }

        // Output f32 PCM audio data
        let samples_needed = buf.len() / 4;
        if self.scratch.len() < samples_needed {
            self.scratch.resize(samples_needed, 0.0);
        }
        self.scratch[..samples_needed].fill(0.0);

        let samples_read = self.bridge.pull_samples(&mut self.scratch[..samples_needed]);
        if count < 10 || count % 500 == 0 {
            tracing::debug!(
                target: "audio_stream",
                "SimpleBridgeReader: samples_read={} for {} needed",
                samples_read,
                samples_needed
            );
        }

        if samples_read == 0 && count % 200 == 0 {
            tracing::debug!(
                target: "audio_stream",
                "SimpleBridgeReader: no samples available from bridge"
            );
        }

        // Convert f32 samples to little-endian bytes
        let mut bytes_written = 0;
        for &sample in self.scratch[..samples_needed].iter() {
            let bytes = sample.to_le_bytes();
            if bytes_written + 4 <= buf.len() {
                buf[bytes_written..bytes_written + 4].copy_from_slice(&bytes);
                bytes_written += 4;
            }
        }

        // Pace the stream here so Songbird can't drain the source too fast.
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

struct Handler {
    guild_id: GuildId,
    channel_id: ChannelId,
    bridge: Arc<AudioBridge>,
    ready_tx: mpsc::Sender<()>,
    presence_rx: Mutex<Option<mpsc::UnboundedReceiver<PresenceUpdate>>>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!("Discord bot connected as {}", ready.user.name);

        // Get the songbird manager
        let manager = songbird::get(&ctx)
            .await
            .expect("Songbird not initialized");

        // Join the voice channel
        match manager.join(self.guild_id, self.channel_id).await {
            Ok(call) => {
                tracing::info!("Joined voice channel!");

                let mut call = call.lock().await;

                // Create simple bridge reader that outputs raw f32 PCM
                let simple_reader = SimpleBridgeReader::new(
                    self.bridge.clone(),
                    self.prebuffer_samples,
                    self.prebuffer_wait,
                );
                tracing::info!("Created SimpleBridgeReader");

                // Wrap with Songbird's RawAdapter which adds the SbirdRaw header
                let raw_adapter = RawAdapter::new(simple_reader, SAMPLE_RATE, CHANNELS);
                tracing::info!(
                    "Created RawAdapter ({}Hz, {} channels)",
                    SAMPLE_RATE,
                    CHANNELS
                );

                let input: Input = raw_adapter.into();

                let track_handle = call.play_only(input.into());

                // Add event handlers to monitor track state
                let _ = track_handle.add_event(
                    Event::Track(TrackEvent::Error),
                    TrackErrorHandler,
                );
                let _ = track_handle.add_event(
                    Event::Track(TrackEvent::End),
                    TrackErrorHandler,
                );

                tracing::info!("Audio source connected to voice channel. Track UUID: {:?}", track_handle.uuid());
            }
            Err(e) => {
                tracing::error!("Failed to join voice channel: {:?}", e);
            }
        }

        // Signal that we're ready
        let _ = self.ready_tx.send(()).await;

        // Start presence updates
        let mut presence_rx = self.presence_rx.lock().await;
        if let Some(rx) = presence_rx.take() {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                run_presence_loop(ctx, rx).await;
            });
        }
    }
}

pub struct DiscordBot {
    client: Client,
    ready_rx: mpsc::Receiver<()>,
}

impl DiscordBot {
    pub async fn new(
        config: &Config,
        bridge: Arc<AudioBridge>,
        presence_rx: mpsc::UnboundedReceiver<PresenceUpdate>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

        let (ready_tx, ready_rx) = mpsc::channel(1);

        let prebuffer_samples =
            (config.prebuffer_seconds * SAMPLE_RATE as f32) as usize * CHANNELS as usize;
        let prebuffer_wait = std::time::Duration::from_secs_f32(
            (config.prebuffer_seconds + 0.5).clamp(0.0, 5.0),
        );
        let handler = Handler {
            guild_id: GuildId::new(config.discord_guild_id),
            channel_id: ChannelId::new(config.discord_channel_id),
            bridge,
            ready_tx,
            presence_rx: Mutex::new(Some(presence_rx)),
            prebuffer_samples,
            prebuffer_wait,
        };

        let client = Client::builder(&config.discord_token, intents)
            .event_handler(handler)
            .register_songbird()
            .await?;

        Ok(Self { client, ready_rx })
    }

    pub async fn start_background(mut self) -> Result<mpsc::Receiver<()>, Box<dyn std::error::Error + Send + Sync>> {
        tokio::spawn(async move {
            if let Err(e) = self.client.start().await {
                tracing::error!("Discord client error: {:?}", e);
            }
        });

        Ok(self.ready_rx)
    }
}

fn status_text(state: &PresenceUpdate, dance_flip: bool) -> String {
    match state {
        PresenceUpdate::Idle => "😴 Just idle".to_string(),
        PresenceUpdate::Paused => "⏸️ On pause".to_string(),
        PresenceUpdate::Playing { title, artist } => {
            let emoji = if dance_flip { "💃" } else { "🕺" };
            let base = format!("{emoji} {title} - {artist}");
            truncate_status(&base, 96)
        }
    }
}

fn truncate_status(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx + 3 >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

async fn set_presence(ctx: &Context, state: &PresenceUpdate, dance_flip: bool) {
    let status_text = status_text(state, dance_flip);
    let activity = ActivityData::custom(status_text);
    let status = match state {
        PresenceUpdate::Playing { .. } => OnlineStatus::Online,
        PresenceUpdate::Paused => OnlineStatus::Idle,
        PresenceUpdate::Idle => OnlineStatus::Idle,
    };
    ctx.set_presence(Some(activity), status);
}

async fn run_presence_loop(
    ctx: Context,
    mut rx: mpsc::UnboundedReceiver<PresenceUpdate>,
) {
    let mut state = PresenceUpdate::Idle;
    let mut dance_flip = false;
    let mut interval = tokio::time::interval(Duration::from_secs(12));
    set_presence(&ctx, &state, dance_flip).await;

    loop {
        tokio::select! {
            Some(update) = rx.recv() => {
                state = update;
                dance_flip = false;
                set_presence(&ctx, &state, dance_flip).await;
            }
            _ = interval.tick(), if matches!(state, PresenceUpdate::Playing { .. }) => {
                dance_flip = !dance_flip;
                set_presence(&ctx, &state, dance_flip).await;
            }
            else => break,
        }
    }
}
