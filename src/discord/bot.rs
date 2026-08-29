use super::presence::run_presence_loop;
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio::generate_join_sound;
use crate::audio::dj::DJAnnouncer;
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::{DeviceAuthorization, SpotifyOAuth};
use crate::presence::PresenceUpdate;
use crate::queue::{PriorityQueue, QueueItem, MediaSource};
use crate::spotify::SpotifyPlayer;
use crate::spotify::SpircCommand;
use crate::youtube::metadata::{fetch_youtube_metadata, validate_attachment};
use crate::youtube::feeder::{feed_youtube_to_bridge, feed_file_to_bridge, FeederError};
use crate::users::{UserCredentials, UserStore};
use serenity::all::{
    ChannelId, CreateCommand, CreateInteractionResponse,
    UserId,
    CreateInteractionResponseMessage, GatewayIntents, GuildId, Interaction, Ready,
};
use serenity::async_trait;
use serenity::builder::{CreateActionRow, CreateButton, CreateCommandOption, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateMessage, EditMessage, EditInteractionResponse};
use serenity::client::{Client, Context, EventHandler};
use serenity::model::application::{ButtonStyle, CommandOptionType};
use serenity::model::id::MessageId;
use serenity::model::voice::VoiceState;
use serenity::model::Timestamp;
use songbird::events::{Event, TrackEvent};
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::SerenityInit;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
// parking_lot: no lock poisoning, so no unwrap_or_else(into_inner)
// incantation at every acquisition (audio_bridge already uses it).
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type ReadySignal = Result<(), String>;

/// Refresh the access token this many seconds before it expires.
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
/// Floor on the proactive-refresh wait, so a short-lived token can't spin.
const TOKEN_REFRESH_MIN_WAIT_SECS: u64 = 30;
/// Backoff after a failed proactive refresh before retrying.
const TOKEN_REFRESH_RETRY_SECS: u64 = 30;
/// Give up the librespot reconnect loop after this many consecutive returns
/// without a healthy session, so a permanently-down Spotify can't hot-loop.
const MAX_SESSION_RESTARTS: u32 = 10;
/// Fallback token lifetime when the real `expires_in` is unknown.
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3600;
/// How long to poll Spotify for a device-code pairing before giving up.
const DEVICE_LOGIN_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Track metadata as delivered by the librespot session itself (no Web API
/// calls — those all 429 under the desktop client ID).
#[derive(Clone)]
struct SpotifyTrackInfo {
    title: String,
    artist: String,
    album_art_url: Option<String>,
}

pub struct ActiveSession {
    pub discord_user_id: u64,
    pub spotify_name: String,
    pub discord_name: String,
    pub access_token: String,
    /// Monotonic id of this spawn, so a task only clears the slot it owns.
    pub generation: u64,
    /// The librespot session task.
    pub handle: JoinHandle<()>,
    /// The proactive token-refresh task that keeps `access_token` current.
    pub refresh_handle: JoinHandle<()>,
}

impl ActiveSession {
    /// Abort both the librespot session and its refresher.
    fn abort(&self) {
        self.handle.abort();
        self.refresh_handle.abort();
    }
}

struct Handler {
    guild_id: GuildId,
    channel_id: ChannelId,
    text_channel_id: ChannelId,
    bridge: Arc<AudioBridge>,
    config: Arc<Config>,
    ready_tx: mpsc::Sender<ReadySignal>,
    presence_rx: Mutex<Option<mpsc::UnboundedReceiver<PresenceUpdate>>>,
    presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    user_store: Arc<UserStore>,
    oauth: Arc<SpotifyOAuth>,
    /// Pending device-code pairings keyed by Discord user; notifying cancels
    /// that user's poll. In-memory only: a restart drops pending pairings and
    /// the user re-runs `/login`.
    pending_auth: Arc<Mutex<HashMap<u64, Arc<tokio::sync::Notify>>>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    /// Serializes spawn_session so two concurrent logins can't orphan a task.
    spawn_lock: Arc<tokio::sync::Mutex<()>>,
    /// Monotonic session-generation counter.
    session_gen: Arc<std::sync::atomic::AtomicU64>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
    ctx: Arc<Mutex<Option<Context>>>,
    controls_message_id: Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: Arc<Mutex<Option<MessageId>>>,
    // YouTube/file playback fields
    ytdlp_available: bool,
    priority_queue: Arc<Mutex<PriorityQueue>>,
    spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    /// True while a queue drain is running. A single owner drains at a time, so
    /// the /play-triggered drain and the eot-driven manager can't race.
    drain_active: Arc<AtomicBool>,
    feeder_cancel: Arc<Mutex<Option<CancellationToken>>>,
    feeder_paused: Arc<AtomicBool>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    /// Metadata of the current Spotify track, kept fresh by the presence
    /// loop so /np can answer for the Spotify baseline too.
    last_spotify_meta: Arc<Mutex<Option<SpotifyTrackInfo>>>,
    /// The Spotify baseline session's playback state, kept fresh by the
    /// presence loop so /np, /queue, and the control buttons can read it
    /// without a Web API call.
    spotify_state: Arc<Mutex<SpotifyState>>,
    /// Whether Spotify was playing when the current/most recent
    /// priority-queue drain started. `run_queue_drain` sets this once per
    /// drain and uses it to decide whether a natural end resumes Spotify;
    /// `handle_skip` reads it for the same decision when a drain is
    /// cancelled instead of ending naturally.
    resume_spotify_after_drain: Arc<AtomicBool>,
    /// Last /play per user, for the metadata-probe cooldown.
    play_cooldowns: Arc<Mutex<HashMap<u64, Instant>>>,
    auto_start_attempted: AtomicBool,
}

/// The Spotify Connect baseline session's playback state, as last reported by
/// a `PresenceUpdate`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpotifyState {
    Idle,
    Playing,
    Paused,
}



fn register_commands(ytdlp_available: bool) -> Vec<CreateCommand> {
    let mut cmds = vec![
        CreateCommand::new("login")
            .description("Connect your Spotify account (or reactivate existing session)"),
        CreateCommand::new("logout")
            .description("Deactivate your Spotify session (credentials kept for quick re-login)"),
        CreateCommand::new("forget")
            .description("Permanently delete your stored Spotify credentials"),
        CreateCommand::new("who")
            .description("Show whose Spotify account is currently active"),
        CreateCommand::new("queue")
            .description("Add to the queue without starting playback; no argument shows the queue")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "url",
                    "Spotify track URL/URI, or YouTube/SoundCloud URL",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::Attachment, "file",
                    "Audio file to queue (mp3, flac, ogg, wav, m4a, aac, opus, wma)")
                .required(false),
            ),
        CreateCommand::new("skip")
            .description("Skip the current track"),
        CreateCommand::new("stop")
            .description("Stop playback and clear the priority queue"),
        CreateCommand::new("np")
            .description("Show what's currently playing"),
        CreateCommand::new("announce")
            .description("Toggle DJ track announcements on/off"),
    ];

    if ytdlp_available {
        cmds.push(
            CreateCommand::new("play")
                .description("Play a Spotify/YouTube/SoundCloud URL or file attachment")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "url",
                        "Spotify, YouTube, or SoundCloud URL")
                    .required(false),
                )
                .add_option(
                    CreateCommandOption::new(CommandOptionType::Attachment, "file",
                        "Audio file to play (mp3, flac, ogg, wav, m4a, aac, opus, wma)")
                    .required(false),
                )
                .add_option(
                    CreateCommandOption::new(CommandOptionType::Boolean, "next",
                        "Play this right after the current track")
                    .required(false),
                ),
        );
    }

    cmds
}

struct CursorSource(std::io::Cursor<Vec<u8>>);

impl Read for CursorSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for CursorSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl songbird::input::core::io::MediaSource for CursorSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}

async fn play_join_sound_then_bridge(
    call_lock: Arc<tokio::sync::Mutex<songbird::Call>>,
    bridge: Arc<AudioBridge>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
    dj: Arc<DJAnnouncer>,
) {
    // Use DJ greeting if available, fall back to beep
    let (stereo_f32, duration_secs) = if let Some(clip) = dj.join_clip() {
        let dur = clip.len() as f64 / (44100.0 * 2.0);
        let scaled: Vec<f32> = clip.iter().map(|s| s * 0.18).collect();
        (scaled, dur)
    } else {
        let join_samples = generate_join_sound();
        let stereo: Vec<f32> = join_samples.iter()
            .flat_map(|&s| { let f = s as f32 / i16::MAX as f32; [f, f] })
            .collect();
        let dur = join_samples.len() as f64 / 44100.0;
        (stereo, dur)
    };
    let bytes: Vec<u8> = stereo_f32.iter().flat_map(|s| s.to_le_bytes()).collect();

    {
        let mut call = call_lock.lock().await;
        let boop_source = CursorSource(std::io::Cursor::new(bytes));
        let raw = RawAdapter::new(boop_source, SAMPLE_RATE, CHANNELS);
        let input: Input = raw.into();
        let _boop_handle = call.play_only(input.into());
    }

    tokio::time::sleep(std::time::Duration::from_secs_f64(duration_secs + 0.1)).await;

    let reader = SimpleBridgeReader::new(bridge, prebuffer_samples, prebuffer_wait);
    let input = reader.into_input();
    let mut call = call_lock.lock().await;
    let track_handle = call.play_only(input.into());
    let _ = track_handle.add_event(Event::Track(TrackEvent::Error), TrackErrorHandler);
    let _ = track_handle.add_event(Event::Track(TrackEvent::End), TrackErrorHandler);
    tracing::info!(track_uuid = ?track_handle.uuid(), "bridge reader connected after join sound");
    let mut lock = track_handle_store.lock();
    *lock = Some(track_handle);
}

// --- Embed builders ---

fn build_now_playing_embed(meta: &SpotifyTrackInfo, track_id: &str, spotify_name: &str) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(0x1DB954u32)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(format!("{} — {}", meta.title, meta.artist))
        .url(format!("https://open.spotify.com/track/{}", track_id))
        .timestamp(Timestamp::now());

    if !spotify_name.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(format!("🎧 {}", spotify_name)));
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.image(art_url);
    }

    embed
}

fn build_history_embed(meta: &SpotifyTrackInfo, track_id: &str, spotify_name: &str) -> CreateEmbed {
    let footer_text = if spotify_name.is_empty() {
        String::new()
    } else {
        format!("played by {}", spotify_name)
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(format!(
            "[{} — {}](https://open.spotify.com/track/{})",
            meta.title, meta.artist, track_id
        ));

    if !footer_text.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(footer_text));
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.thumbnail(art_url);
    }

    embed
}

fn build_priority_now_playing_embed(item: &QueueItem) -> CreateEmbed {
    let color = item.source.embed_color();
    let title = item.source.display_title();
    let subtitle = item.source.display_subtitle();
    let footer_icon = match &item.source {
        MediaSource::YouTube { .. } => "🎬",
        MediaSource::File { .. } => "📎",
    };

    let footer_text = match item.source.display_duration() {
        Some(d) => format!("{} {} · {}", footer_icon, item.queued_by, d),
        None => format!("{} {}", footer_icon, item.queued_by),
    };
    let mut embed = CreateEmbed::new()
        .color(color)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(format!("{} — {}", title, subtitle))
        .timestamp(Timestamp::now())
        .footer(CreateEmbedFooter::new(footer_text));

    if let MediaSource::YouTube { video_id, thumbnail_url, .. } = &item.source {
        let url = format!("https://www.youtube.com/watch?v={}", video_id);
        embed = embed.url(url);
        if let Some(thumb) = thumbnail_url {
            embed = embed.image(thumb);
        }
    }

    embed
}

fn build_priority_history_embed(item: &QueueItem) -> CreateEmbed {
    let footer_text = match item.source.display_duration() {
        Some(d) => format!("played by {} · {}", item.queued_by, d),
        None => format!("played by {}", item.queued_by),
    };
    let description = match &item.source {
        MediaSource::YouTube { title, channel, video_id, .. } => {
            format!("[{} — {}](https://www.youtube.com/watch?v={})", title, channel, video_id)
        }
        MediaSource::File { filename, .. } => {
            format!("📎 {}", filename)
        }
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(description)
        .footer(CreateEmbedFooter::new(footer_text));

    if let MediaSource::YouTube { thumbnail_url: Some(thumb), .. } = &item.source {
        embed = embed.thumbnail(thumb);
    }

    embed
}

/// The idle controls card. Once a track is playing, the now-playing embed
/// (which carries its own buttons) supersedes this, so there is no separate
/// "is playing" state to render here.
fn build_controls_embed(active_user: Option<&str>) -> CreateEmbed {
    match active_user {
        Some(name) => CreateEmbed::new()
            .color(0x1DB954u32)
            .title(format!("🎛️ {}", name))
            .description("*Play something to get started!*"),
        None => CreateEmbed::new()
            .color(0x5865F2u32)
            .title("🎛️ Spotibot")
            .description("*Use `/login` to start a session*"),
    }
}

fn build_controls_buttons(is_paused: bool) -> CreateActionRow {
    let pause_label = if is_paused { "▶" } else { "⏸" };
    CreateActionRow::Buttons(vec![
        CreateButton::new("ctrl_prev").label("⏮").style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_pause_toggle").label(pause_label).style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_next").label("⏭").style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_queue_hint").label("➕ Queue").style(ButtonStyle::Secondary),
    ])
}

async fn post_controls(ctx: &Context, text_channel_id: ChannelId, active_user: Option<&str>) -> Option<MessageId> {
    let embed = build_controls_embed(active_user);
    let mut msg = CreateMessage::new().embed(embed);
    if active_user.is_some() {
        msg = msg.components(vec![build_controls_buttons(false)]);
    }
    match text_channel_id.send_message(ctx, msg).await {
        Ok(m) => {
            tracing::info!("posted controls message");
            Some(m.id)
        }
        Err(e) => {
            tracing::warn!(error = ?e, "failed to post controls message");
            None
        }
    }
}

async fn delete_and_repost_controls(
    ctx: &Context,
    text_channel_id: ChannelId,
    controls_message_id: &Arc<Mutex<Option<MessageId>>>,
    active_user: Option<&str>,
) {
    let old_id = {
        let lock = controls_message_id.lock();
        *lock
    };
    if let Some(mid) = old_id {
        let _ = text_channel_id.delete_message(ctx, mid).await;
    }

    let new_id = post_controls(ctx, text_channel_id, active_user).await;
    let mut lock = controls_message_id.lock();
    *lock = new_id;
}

/// Parse a Spotify track ID from a URL or URI.
/// Accepts `spotify:track:<id>` and any `open.spotify.com` URL with a
/// `/track/<id>` path segment, including locale-prefixed links
/// (`open.spotify.com/intl-fr/track/<id>`).
fn parse_track_id_from_url(input: &str) -> Option<String> {
    let input = input.trim();
    let candidate = if let Some(rest) = input.strip_prefix("spotify:track:") {
        rest.split('?').next().unwrap_or(rest)
    } else if input.contains("open.spotify.com/") {
        let after = input.split("/track/").nth(1)?;
        after.split(['?', '/', '#']).next().unwrap_or(after)
    } else {
        return None;
    };
    is_valid_track_id(candidate).then(|| candidate.to_string())
}

/// Spotify track IDs are exactly 22 base62 characters. Rejecting anything
/// else keeps user input out of the query string of authenticated API calls.
fn is_valid_track_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Result of sorting a `/play` or `/queue` link argument into the Spotify
/// fast path or the generic YouTube/SoundCloud/attachment path.
enum LinkKind {
    Spotify(librespot_core::SpotifyUri),
    Other,
}

/// Classifies a URL/URI argument. A recognized Spotify track link resolves
/// straight to a `SpotifyUri`; anything else (including a malformed Spotify
/// link) falls through to the YouTube/SoundCloud/attachment path, which
/// reports its own "unsupported URL" error for garbage input.
fn classify_link(input: &str) -> LinkKind {
    let track_id = match parse_track_id_from_url(input) {
        Some(id) => id,
        None => return LinkKind::Other,
    };
    let uri = format!("spotify:track:{}", track_id);
    match librespot_core::SpotifyUri::from_uri(&uri) {
        Ok(u) => LinkKind::Spotify(u),
        Err(e) => {
            tracing::warn!(error = %e, uri = %uri, "failed to parse Spotify track URI");
            LinkKind::Other
        }
    }
}

// --- Priority queue embed posting helpers ---

async fn post_priority_now_playing(
    ctx_store: &Arc<Mutex<Option<Context>>>,
    text_channel_id: ChannelId,
    item: &QueueItem,
    controls_message_id: &Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: &Arc<Mutex<Option<MessageId>>>,
) {
    let ctx = {
        let lock = ctx_store.lock();
        match lock.clone() { Some(c) => c, None => return }
    };

    // Delete previous now-playing
    let prev_np = {
        let lock = now_playing_message_id.lock();
        *lock
    };
    if let Some(mid) = prev_np {
        let _ = text_channel_id.delete_message(&ctx, mid).await;
    }

    // Delete old controls
    let old_ctrl = {
        let lock = controls_message_id.lock();
        *lock
    };
    if let Some(mid) = old_ctrl {
        // Only delete if different from now-playing (they may be the same message)
        if prev_np != Some(mid) {
            let _ = text_channel_id.delete_message(&ctx, mid).await;
        }
    }

    let embed = build_priority_now_playing_embed(item);
    let buttons = build_controls_buttons(false);
    let msg = CreateMessage::new().embed(embed).components(vec![buttons]);

    match text_channel_id.send_message(&ctx, msg).await {
        Ok(m) => {
            let mut np_lock = now_playing_message_id.lock();
            *np_lock = Some(m.id);
            let mut ctrl_lock = controls_message_id.lock();
            *ctrl_lock = Some(m.id);
        }
        Err(e) => tracing::warn!(error = ?e, "failed to send priority now-playing"),
    }
}

async fn post_priority_history(
    ctx_store: &Arc<Mutex<Option<Context>>>,
    text_channel_id: ChannelId,
    item: &QueueItem,
) {
    let ctx = {
        let lock = ctx_store.lock();
        match lock.clone() { Some(c) => c, None => return }
    };

    let embed = build_priority_history_embed(item);
    let msg = CreateMessage::new().embed(embed);
    let _ = text_channel_id.send_message(&ctx, msg).await;
}

// --- Priority queue manager ---

/// Clears the drain-active flag on drop, so an aborted or panicking drain task
/// can't leave the flag stuck true (which would block all future drains).
struct DrainGuard(Arc<AtomicBool>);

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Everything one queue-drain pass needs. Built by each drain owner (the
/// eot-driven manager and the /play-triggered drain) so both entry points run
/// the same implementation and cannot diverge.
struct QueueDrainCtx {
    priority_queue: Arc<Mutex<PriorityQueue>>,
    bridge: Arc<AudioBridge>,
    spirc_cmd_tx: Option<mpsc::UnboundedSender<SpircCommand>>,
    ctx: Arc<Mutex<Option<Context>>>,
    text_channel_id: ChannelId,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    feeder_cancel: Arc<Mutex<Option<CancellationToken>>>,
    feeder_paused: Arc<AtomicBool>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    controls_message_id: Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: Arc<Mutex<Option<MessageId>>>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
    spotify_state: Arc<Mutex<SpotifyState>>,
    resume_spotify_after_drain: Arc<AtomicBool>,
}

/// Drain the priority queue until it is empty or the current item is
/// cancelled. The caller must already own the drain-active flag (DrainGuard).
///
/// Semantics decided once for both owners: no history embed for cancelled or
/// failed items (failures get a user-facing error message instead); Spotify
/// resumes only after a natural drain; a cancelled drain resumes nothing —
/// skip/stop owns what plays next.
async fn run_queue_drain(d: &QueueDrainCtx) {
    // Only resume Spotify at the end of a natural drain if it was actually
    // playing when the drain started — an idle/paused baseline should stay
    // that way, not spring back to life because a queued item finished.
    // Recorded into the shared flag too, so a cancelled (skipped) drain's
    // caller (handle_skip) can make the same decision.
    let resume_spotify = *d.spotify_state.lock() == SpotifyState::Playing;
    d.resume_spotify_after_drain.store(resume_spotify, Ordering::SeqCst);
    let mut cancelled = false;
    loop {
        let item = {
            let mut lock = d.priority_queue.lock();
            lock.pop()
        };
        let item = match item {
            Some(i) => i,
            None => break,
        };

        // Mark the item active BEFORE pausing Spotify: the pause lands as a
        // Paused/Idle player event, and the presence loop pauses the shared
        // bridge-reader track unless it sees an active priority item — which
        // would leave the whole item playing into a paused output.
        {
            let mut lock = d.active_priority_item.lock();
            *lock = Some(item.clone());
        }

        if let Some(ref tx) = d.spirc_cmd_tx {
            if tx.send(SpircCommand::Pause).is_ok() {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
        d.bridge.clear();

        // The bridge-reader track may already be paused (Spotify was paused
        // or idle before this drain); resume it so the feeder is heard.
        {
            let handle = {
                let lock = d.track_handle.lock();
                lock.clone()
            };
            if let Some(h) = handle {
                let _ = h.play();
            }
        }

        post_priority_now_playing(
            &d.ctx, d.text_channel_id, &item,
            &d.controls_message_id, &d.now_playing_message_id,
        ).await;

        // DJ announcement before track (honors the /announce toggle)
        if d.announce_enabled.load(Ordering::Relaxed) && d.dj.is_enabled() {
            let title = item.source.display_title().to_string();
            let subtitle = item.source.display_subtitle().to_string();
            let queued_by = item.queued_by.clone();
            if let Some(clip) = d.dj.track_announce_clip(&title, &subtitle, &queued_by).await {
                d.bridge.push_overlay(&clip);
            }
        }

        let token = CancellationToken::new();
        {
            let mut lock = d.feeder_cancel.lock();
            *lock = Some(token.clone());
        }
        d.feeder_paused.store(false, Ordering::Relaxed);

        let feed_result = match &item.source {
            MediaSource::YouTube { url, .. } => {
                feed_youtube_to_bridge(url, d.bridge.clone(), token, d.feeder_paused.clone()).await
            }
            MediaSource::File { attachment_url, filename, .. } => {
                let ext = filename.rsplit('.').next().unwrap_or("mp3");
                feed_file_to_bridge(attachment_url, ext, d.bridge.clone(), token, d.feeder_paused.clone()).await
            }
        };

        {
            let mut lock = d.active_priority_item.lock();
            *lock = None;
        }

        match feed_result {
            Ok(()) => {
                tracing::info!("priority item finished: {}", item.source.display_title());
                post_priority_history(&d.ctx, d.text_channel_id, &item).await;
            }
            Err(FeederError::Cancelled) => {
                tracing::info!("priority item cancelled (skip/stop)");
                cancelled = true;
                break;
            }
            Err(e) => {
                tracing::warn!("feeder error: {}", e);
                let ctx = {
                    let lock = d.ctx.lock();
                    lock.clone()
                };
                if let Some(ctx) = ctx {
                    let msg = CreateMessage::new().content(format!(
                        "⚠️ <@{}> Couldn't play **{}** — the download or decode failed.",
                        item.queued_by_id,
                        item.source.display_title()
                    ));
                    let _ = d.text_channel_id.send_message(&ctx, msg).await;
                }
            }
        }
    }

    if cancelled {
        return;
    }

    // Natural drain end: hand playback back to Spotify, but only if it was
    // playing before this drain started. With no live session to resume (or
    // nothing to resume to), delete the last Now Playing card instead of
    // leaving it in the channel with dead buttons.
    let resumed = resume_spotify
        && d
            .spirc_cmd_tx
            .as_ref()
            .map(|tx| tx.send(SpircCommand::Play).is_ok())
            .unwrap_or(false);
    if !resumed {
        let ctx = {
            let lock = d.ctx.lock();
            lock.clone()
        };
        if let Some(ctx) = ctx {
            let np = {
                let mut lock = d.now_playing_message_id.lock();
                lock.take()
            };
            let ctrl = {
                let lock = d.controls_message_id.lock();
                *lock
            };
            if let Some(mid) = np {
                if ctrl != Some(mid) {
                    let _ = d.text_channel_id.delete_message(&ctx, mid).await;
                }
            }
            delete_and_repost_controls(&ctx, d.text_channel_id, &d.controls_message_id, None).await;
        }
    }
}

// Wide orchestration fn wiring together the queue, bridge, and UI state.
#[allow(clippy::too_many_arguments)]
async fn priority_queue_manager(
    mut end_of_track_rx: mpsc::UnboundedReceiver<()>,
    priority_queue: Arc<Mutex<PriorityQueue>>,
    bridge: Arc<AudioBridge>,
    spirc_cmd_tx: mpsc::UnboundedSender<SpircCommand>,
    ctx: Arc<Mutex<Option<Context>>>,
    text_channel_id: ChannelId,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    feeder_cancel: Arc<Mutex<Option<CancellationToken>>>,
    feeder_paused: Arc<AtomicBool>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    drain_active: Arc<AtomicBool>,
    controls_message_id: Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: Arc<Mutex<Option<MessageId>>>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
    spotify_state: Arc<Mutex<SpotifyState>>,
    resume_spotify_after_drain: Arc<AtomicBool>,
) {
    let drain_ctx = QueueDrainCtx {
        priority_queue,
        bridge,
        spirc_cmd_tx: Some(spirc_cmd_tx),
        ctx,
        text_channel_id,
        active_priority_item,
        feeder_cancel,
        feeder_paused,
        dj,
        announce_enabled,
        controls_message_id,
        now_playing_message_id,
        track_handle,
        spotify_state,
        resume_spotify_after_drain,
    };
    loop {
        match end_of_track_rx.recv().await {
            Some(()) => {}
            None => {
                tracing::debug!("priority queue manager: channel closed, exiting");
                return;
            }
        }

        // Only one drain runs at a time. If a /play-triggered drain already
        // owns it, that drain will pick up whatever is queued — skip.
        if drain_active.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            continue;
        }
        let _drain_guard = DrainGuard(drain_active.clone());
        run_queue_drain(&drain_ctx).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_presence_loop_with_track(
    ctx: Context,
    mut rx: mpsc::UnboundedReceiver<PresenceUpdate>,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    text_channel_id: ChannelId,
    controls_message_id: Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: Arc<Mutex<Option<MessageId>>>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    bridge: Arc<AudioBridge>,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    last_meta_store: Arc<Mutex<Option<SpotifyTrackInfo>>>,
    spotify_state: Arc<Mutex<SpotifyState>>,
) {
    let (fwd_tx, fwd_rx) = mpsc::unbounded_channel::<PresenceUpdate>();
    let ctx_presence = ctx.clone();
    tokio::spawn(async move {
        run_presence_loop(ctx_presence, fwd_rx).await;
    });

    let mut last_track_key: Option<String> = None;
    let mut last_meta: Option<SpotifyTrackInfo> = None;
    let mut last_track_id: String = String::new();
    let mut last_spotify_name: String = String::new();
    let mut is_paused: bool = false;

    while let Some(update) = rx.recv().await {
        let _ = fwd_tx.send(update.clone());

        {
            // The bridge-reader track is shared with priority (YouTube/file)
            // playback. Only pause it on Spotify pause/idle when no priority
            // item is active — otherwise we'd starve the feeder's audio.
            let priority_active = {
                let lock = active_priority_item.lock();
                lock.is_some()
            };
            let lock = track_handle_store.lock();
            if let Some(handle) = lock.as_ref() {
                match &update {
                    PresenceUpdate::Playing { .. } => { let _ = handle.play(); }
                    PresenceUpdate::Paused { .. } | PresenceUpdate::Idle => {
                        if !priority_active { let _ = handle.pause(); }
                    }
                }
            }
        }

        let was_paused = is_paused;
        match &update {
            PresenceUpdate::Paused { .. } => { is_paused = true; }
            PresenceUpdate::Playing { .. } => { is_paused = false; }
            _ => {}
        }
        if was_paused != is_paused {
            let msg_id = {
                let lock = controls_message_id.lock();
                *lock
            };
            if let Some(mid) = msg_id {
                let buttons = build_controls_buttons(is_paused);
                let edit = EditMessage::new().components(vec![buttons]);
                let _ = text_channel_id.edit_message(&ctx, mid, edit).await;
            }
        }

        // Keep the shared playback state in sync with every update, not only
        // the ones that flip the controls buttons — /np, /queue, and the
        // control buttons all read it directly.
        {
            let new_state = match &update {
                PresenceUpdate::Idle => SpotifyState::Idle,
                PresenceUpdate::Paused { .. } => SpotifyState::Paused,
                PresenceUpdate::Playing { .. } => SpotifyState::Playing,
            };
            let mut lock = spotify_state.lock();
            *lock = new_state;
        }

        if let PresenceUpdate::Playing { title, artist, track_id, album_art_url } = &update {
            // Dedup on the track id (stable across replays of the same title);
            // fall back to title — artist only when the id is missing.
            let track_key = if track_id.is_empty() {
                format!("{} — {}", title, artist)
            } else {
                track_id.clone()
            };
            if last_track_key.as_deref() != Some(&track_key) {
                last_track_key = Some(track_key.clone());

                let spotify_name = {
                    let lock = active_session.lock();
                    lock.as_ref().map(|s| s.discord_name.clone()).unwrap_or_default()
                };

                let prev_msg_id = {
                    let lock = now_playing_message_id.lock();
                    *lock
                };
                if let Some(mid) = prev_msg_id {
                    let _ = text_channel_id.delete_message(&ctx, mid).await;
                    if let Some(ref meta) = last_meta {
                        let history_embed = build_history_embed(meta, &last_track_id, &last_spotify_name);
                        let msg = CreateMessage::new().embed(history_embed);
                        let _ = text_channel_id.send_message(&ctx, msg).await;
                    }
                }

                // Metadata now comes straight from the librespot session
                // (the PresenceUpdate); there is no Web API fallback fetch.
                let meta = SpotifyTrackInfo {
                    title: title.clone(),
                    artist: artist.clone(),
                    album_art_url: album_art_url.clone(),
                };

                let embed = build_now_playing_embed(&meta, track_id, &spotify_name);
                let buttons = build_controls_buttons(false);
                let msg = CreateMessage::new().embed(embed).components(vec![buttons]);

                {
                    let old_ctrl = {
                        let lock = controls_message_id.lock();
                        *lock
                    };
                    if let Some(mid) = old_ctrl {
                        let _ = text_channel_id.delete_message(&ctx, mid).await;
                    }
                }

                match text_channel_id.send_message(&ctx, msg).await {
                    Ok(m) => {
                        tracing::info!(title = %title, artist = %artist, "now-playing embed sent");
                        let mut lock = now_playing_message_id.lock();
                        *lock = Some(m.id);
                        let mut ctrl_lock = controls_message_id.lock();
                        *ctrl_lock = Some(m.id);
                    }
                    Err(e) => tracing::warn!(error = ?e, "failed to send now-playing embed"),
                }

                {
                    let mut lock = last_meta_store.lock();
                    *lock = Some(meta.clone());
                }
                last_meta = Some(meta);
                last_track_id = track_id.clone();
                last_spotify_name = spotify_name;

                // DJ announcement AFTER embed (non-blocking, only if enabled)
                if announce_enabled.load(Ordering::Relaxed) {
                let dj_title = title.clone();
                let dj_artist = artist.clone();
                let dj_ref = dj.clone();
                let bridge_ref = bridge.clone();
                tokio::spawn(async move {
                    match dj_ref.track_announce_clip(&dj_title, &dj_artist, "").await {
                        Some(clip) => {
                            tracing::info!(title = %dj_title, artist = %dj_artist, samples = clip.len(), "DJ overlay pushed");
                            bridge_ref.push_overlay(&clip);
                        }
                        None => {
                            tracing::warn!(title = %dj_title, artist = %dj_artist, "DJ clip failed");
                        }
                    }
                });
                } // end announce_enabled check
            }
        } else if matches!(update, PresenceUpdate::Idle | PresenceUpdate::Paused { .. }) {
            // Clear the dedup key on pause/stop so resuming the same track
            // reposts the now-playing card instead of being swallowed.
            last_track_key = None;
            // Idle means nothing is loaded — drop the stale track metadata so
            // /np and /queue don't describe a track that finished.
            if matches!(update, PresenceUpdate::Idle) {
                let mut lock = last_meta_store.lock();
                *lock = None;
                last_meta = None;
            }
            // A session that comes up already paused (bot restart mid-pause)
            // never sends Playing, so /np would show nothing — seed it here.
            if let PresenceUpdate::Paused { title, artist, track_id } = &update {
                if last_meta.is_none() {
                    let meta = SpotifyTrackInfo {
                        title: title.clone(),
                        artist: artist.clone(),
                        album_art_url: None,
                    };
                    *last_meta_store.lock() = Some(meta.clone());
                    last_meta = Some(meta);
                    last_track_id = track_id.clone();
                }
            }
        }
    }
}

async fn startup_controls(
    ctx: &Context,
    text_channel_id: ChannelId,
    bot_id: serenity::model::id::UserId,
    controls_message_id: &Arc<Mutex<Option<MessageId>>>,
) {
    use serenity::all::GetMessages;
    let builder = GetMessages::new().limit(20);
    if let Ok(messages) = text_channel_id.messages(ctx, builder).await {
        for msg in &messages {
            if msg.author.id != bot_id {
                continue;
            }
            // A stale control/now-playing message is any of ours that still
            // carries buttons, or whose embed is one of our control cards
            // (idle "🎛️ Spotibot" or an active "🎛️ {name}"). Matching on the
            // buttons catches the merged now-playing card too, whose title is
            // the track name rather than a "🎛️" string.
            let has_buttons = !msg.components.is_empty();
            let is_control_card = msg
                .embeds
                .iter()
                .any(|e| e.title.as_deref().is_some_and(|t| t.starts_with("🎛️")));
            if has_buttons || is_control_card {
                let _ = text_channel_id.delete_message(ctx, msg.id).await;
            }
        }
    }

    let new_id = post_controls(ctx, text_channel_id, None).await;
    let mut lock = controls_message_id.lock();
    *lock = new_id;
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "discord bot connected");

        match self.guild_id.set_commands(&ctx, register_commands(self.ytdlp_available)).await {
            Ok(cmds) => tracing::info!("registered {} slash commands", cmds.len()),
            Err(e) => tracing::warn!(error = ?e, "failed to register slash commands"),
        }

        {
            let mut ctx_store = self.ctx.lock();
            *ctx_store = Some(ctx.clone());
        }
        // try_send: main() consumes exactly one ready signal then parks, so a
        // blocking send on a later gateway resume would park this handler
        // task forever (one leaked task per resume).
        let _ = self.ready_tx.try_send(Ok(()));

        // First-ready-only work. Discord re-fires ready() after every gateway
        // resume/reconnect; reposting controls then would orphan the live
        // controls message and clobber controls_message_id mid-playback.
        let first_ready = !self.auto_start_attempted.swap(true, Ordering::SeqCst);
        if first_ready {
            // Awaited, not detached: auto_start_stored_session below also
            // writes controls_message_id, and a detached startup post
            // finishing second would orphan the active-user card.
            startup_controls(&ctx, self.text_channel_id, ready.user.id, &self.controls_message_id).await;
        }

        let rx_taken = {
            let mut presence_rx = self.presence_rx.lock();
            presence_rx.take()
        };
        if let Some(rx) = rx_taken {
            let ctx_presence = ctx.clone();
            let track_handle_store = self.track_handle.clone();
            let active_session = self.active_session.clone();
            let text_channel_id = self.text_channel_id;
            let controls_id = self.controls_message_id.clone();
            let np_id = self.now_playing_message_id.clone();
            let dj_presence = self.dj.clone();
            let bridge_presence = self.bridge.clone();
            let announce_presence = self.announce_enabled.clone();
            let priority_item = self.active_priority_item.clone();
            let last_meta_store = self.last_spotify_meta.clone();
            let spotify_state = self.spotify_state.clone();
            tokio::spawn(async move {
                run_presence_loop_with_track(
                    ctx_presence, rx, track_handle_store, active_session,
                    text_channel_id, controls_id, np_id,
                    dj_presence, announce_presence,
                    bridge_presence, priority_item,
                    last_meta_store, spotify_state,
                ).await;
            });
        }

        // Auto-start: replay the stored active user's session through the same
        // machinery /login uses (voice join, controls, priority queue, refresh
        // loop). Runs only on the first ready (see `first_ready` above).
        if first_ready {
            self.auto_start_stored_session().await;
        }
    }

    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        if new.guild_id != Some(self.guild_id) {
            return;
        }
        // The bot's own join events are ignored (its join to an empty channel
        // on auto-start would otherwise fire the empty-channel teardown and
        // kill the session it just started) — but its own DISCONNECT must
        // tear the session down, or an admin force-disconnect leaves
        // librespot and the feeder pushing into a dead call forever.
        if new.user_id == ctx.cache.current_user().id {
            if new.channel_id.is_none() {
                let anything_active = {
                    let session = {
                        let lock = self.active_session.lock();
                        lock.is_some()
                    };
                    let priority = {
                        let lock = self.active_priority_item.lock();
                        lock.is_some()
                    };
                    session || priority || self.drain_active.load(Ordering::SeqCst)
                };
                if anything_active {
                    tracing::info!("bot disconnected from voice — tearing down playback");
                    self.teardown_playback_session(&ctx, false).await;
                }
            }
            return;
        }

        let (bot_channel, humans_in_bot_channel) = {
            let bot_id = ctx.cache.current_user().id;
            match self.guild_id.to_guild_cached(&ctx) {
                Some(guild) => {
                    let bot_ch = guild.voice_states.get(&bot_id).and_then(|vs| vs.channel_id);
                    let humans = match bot_ch {
                        Some(ch) => guild
                            .voice_states
                            .values()
                            .filter(|vs| vs.channel_id == Some(ch))
                            .filter(|vs| vs.user_id != bot_id)
                            .filter(|vs| guild.members.get(&vs.user_id).map(|m| !m.user.bot).unwrap_or(true))
                            .count(),
                        None => return,
                    };
                    (bot_ch, humans)
                }
                None => return,
            }
        };

        tracing::debug!(humans_in_bot_channel, ?bot_channel, "voice state checked");

        // Empty channel means full teardown and leave — regardless of whether
        // a Spotify session exists. Gating this on a session used to let
        // YouTube/file-only playback (started via /play with no /login) keep
        // playing to an empty channel forever.
        if humans_in_bot_channel == 0 {
            tracing::info!("voice channel empty — tearing down playback");
            self.teardown_playback_session(&ctx, true).await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Component(component) = &interaction {
            let custom_id = component.data.custom_id.as_str();
            tracing::debug!(custom_id, "button interaction received");

            let spirc_tx = {
                let lock = self.spirc_cmd_tx.lock();
                lock.clone()
            };

            let priority_playing = {
                let lock = self.active_priority_item.lock();
                lock.is_some()
            };

            // Control buttons require sharing the bot's voice channel. The
            // queue-hint button is read-only info, so it stays open.
            let is_control = matches!(custom_id, "ctrl_prev" | "ctrl_next" | "ctrl_pause_toggle");
            if is_control && !self.user_in_bot_voice_channel(&ctx, component.user.id) {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("You must be in the bot's voice channel to control playback.")
                        .ephemeral(true),
                );
                let _ = component.create_response(&ctx, response).await;
                return;
            }

            let reply_content: String = match custom_id {
                "ctrl_prev" => {
                    if priority_playing {
                        // Sending Spotify "previous" here would silently move
                        // the paused baseline session under the active item.
                        "⏮ Previous isn't available during queue playback.".to_string()
                    } else if let Some(tx) = &spirc_tx {
                        if tx.send(SpircCommand::Previous).is_ok() {
                            "⏮ Previous".to_string()
                        } else {
                            "⚠ Spotify didn't accept that — try again.".to_string()
                        }
                    } else {
                        "No active Spotify session.".to_string()
                    }
                }
                // Same semantics as /skip: cancel the current priority item
                // and either continue the queue or resume Spotify.
                "ctrl_next" => self.handle_skip().await,
                "ctrl_pause_toggle" => {
                    if priority_playing {
                        let current = self.feeder_paused.load(Ordering::Relaxed);
                        let new_paused = !current;
                        self.feeder_paused.store(new_paused, Ordering::Relaxed);
                        // Update the button visual to reflect pause/play state.
                        let msg_id = {
                            let lock = self.controls_message_id.lock();
                            *lock
                        };
                        if let Some(mid) = msg_id {
                            let buttons = build_controls_buttons(new_paused);
                            let edit = EditMessage::new().components(vec![buttons]);
                            let _ = self.text_channel_id.edit_message(&ctx, mid, edit).await;
                        }
                        if current { "▶ Resumed".to_string() } else { "⏸ Paused".to_string() }
                    } else if let Some(tx) = &spirc_tx {
                        let state = *self.spotify_state.lock();
                        if state == SpotifyState::Playing {
                            let ok = tx.send(SpircCommand::Pause).is_ok();
                            if ok { "⏸ Paused".to_string() } else { "⚠ Spotify didn't accept that — try again.".to_string() }
                        } else {
                            // ▶ pressed while Spotify isn't playing: a
                            // non-empty priority queue takes precedence over
                            // resuming the Spotify baseline.
                            let queue_non_empty = {
                                let lock = self.priority_queue.lock();
                                !lock.is_empty()
                            };
                            if queue_non_empty {
                                match self.trigger_priority_queue_drain(None).await {
                                    Ok(_) => "▶ Playing from queue".to_string(),
                                    Err(msg) => format!("⚠ {}", msg),
                                }
                            } else {
                                let ok = tx.send(SpircCommand::Play).is_ok();
                                if ok { "▶ Resumed".to_string() } else { "⚠ Spotify didn't accept that — try again.".to_string() }
                            }
                        }
                    } else {
                        "No active Spotify session.".to_string()
                    }
                }
                "ctrl_queue_hint" => {
                    let content = self.format_queue_listing();

                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content(content).ephemeral(true),
                    );
                    if let Err(e) = component.create_response(&ctx, response).await {
                        tracing::warn!(error = ?e, "failed to respond to button interaction");
                    }
                    return;
                }
                _ => "Unknown button".to_string(),
            };

            if custom_id != "ctrl_queue_hint" {
                // Ephemeral reply: only the clicker sees the outcome, no channel spam.
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(reply_content)
                        .ephemeral(true),
                );
                if let Err(e) = component.create_response(&ctx, response).await {
                    tracing::warn!(error = ?e, "failed to respond to button interaction");
                }
            }
            return;
        }

        let cmd = match interaction.command() {
            Some(c) => c,
            None => { tracing::warn!("interaction was not a command or component"); return; }
        };
        tracing::debug!(command = %cmd.data.name, "processing slash command");

        let user_id = cmd.user.id.to_string();
        let user_id_u64 = cmd.user.id.get();
        let username = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let in_voice = self.user_in_bot_voice_channel(&ctx, cmd.user.id);

        // Handle /play separately (deferred response)
        if cmd.data.name.as_str() == "play" {
            if !self.user_can_play(&ctx, cmd.user.id) {
                let _ = cmd.create_response(&ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("Join a voice channel first (or the bot's channel if it's already in one) to queue playback.")
                        .ephemeral(true),
                )).await;
                return;
            }
            self.handle_play(&cmd, &ctx).await;
            return;
        }

        // Handle /queue separately too: an "Other" (YT/SC/attachment) link
        // spawns the same yt-dlp metadata probe /play does, so it needs the
        // same deferred-response treatment.
        if cmd.data.name.as_str() == "queue" {
            if !in_voice {
                let _ = cmd.create_response(&ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("You must be in the bot's voice channel to control playback.")
                        .ephemeral(true),
                )).await;
                return;
            }
            self.handle_queue(&cmd, &ctx).await;
            return;
        }

        // Defer login immediately — OAuth + session startup takes >3s
        if cmd.data.name.as_str() == "login" {
            let _ = cmd.defer_ephemeral(&ctx).await;
            match self.handle_login(&user_id, user_id_u64, &username, in_voice).await {
                LoginOutcome::Reply(s) => {
                    let _ = cmd.edit_response(&ctx, serenity::builder::EditInteractionResponse::new().content(s)).await;
                }
                LoginOutcome::Pair(auth) => {
                    let _ = cmd.edit_response(
                        &ctx,
                        serenity::builder::EditInteractionResponse::new().content(format!(
                            "Go to <{}> and enter code **{}**.\nThis code expires in 10 minutes.",
                            auth.url(),
                            auth.user_code
                        )),
                    ).await;
                    // Serenity dispatches each interaction in its own task, so
                    // this long await (up to DEVICE_LOGIN_MAX_WAIT) doesn't
                    // block other events.
                    let reply = self.finish_device_login(&user_id, user_id_u64, &username, &ctx, auth).await;
                    let _ = cmd.edit_response(&ctx, serenity::builder::EditInteractionResponse::new().content(reply)).await;
                }
            }
            return;
        }

        // Commands that drive playback require sharing the bot's voice channel.
        // /announce is a guild-level toggle, not playback control, and must be
        // settable before the bot is in voice.
        let needs_voice = matches!(cmd.data.name.as_str(), "skip" | "stop");
        if needs_voice && !in_voice {
            let _ = cmd.create_response(&ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("You must be in the bot's voice channel to control playback.")
                    .ephemeral(true),
            )).await;
            return;
        }

        let reply = match cmd.data.name.as_str() {
            "login" => unreachable!(),
            "logout" => self.handle_logout(&user_id, user_id_u64).await,
            "forget" => self.handle_forget(&user_id, user_id_u64).await,
            "who" => self.handle_who().await,
            "skip" => self.handle_skip().await,
            "stop" => self.handle_stop().await,
            "np" => self.handle_np().await,
            "announce" => self.handle_announce().await,
            _ => return,
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(reply).ephemeral(true),
        );

        if let Err(e) = cmd.create_response(&ctx, response).await {
            tracing::warn!(error = ?e, "failed to create interaction response");
        }
    }
}

/// Outcome of `/login`: either a plain reply, or a freshly issued device-code
/// pairing that the caller must show the user and then poll to completion.
enum LoginOutcome {
    Reply(String),
    Pair(DeviceAuthorization),
}

/// Pure voice-gate policy (PORT.md locked decision 4): with the bot in a
/// channel the user must share it; with the bot in none, `allow_follow`
/// decides whether being in any voice channel suffices (the /play
/// fresh-boot path, where the bot joins the requester).
fn voice_gate(
    bot_ch: Option<ChannelId>,
    user_ch: Option<ChannelId>,
    allow_follow: bool,
) -> bool {
    match bot_ch {
        Some(bc) => user_ch == Some(bc),
        None => allow_follow && user_ch.is_some(),
    }
}

impl Handler {
    /// The bot's and the given user's current voice channels, from the cache.
    fn voice_channels(&self, ctx: &Context, user_id: UserId) -> (Option<ChannelId>, Option<ChannelId>) {
        let bot_id = ctx.cache.current_user().id;
        match self.guild_id.to_guild_cached(ctx) {
            Some(guild) => (
                guild.voice_states.get(&bot_id).and_then(|vs| vs.channel_id),
                guild.voice_states.get(&user_id).and_then(|vs| vs.channel_id),
            ),
            None => (None, None),
        }
    }

    /// nob's control rule: a member may drive playback only while sharing the
    /// bot's voice channel. False when the bot isn't in a channel, the user
    /// isn't in one, or they differ.
    fn user_in_bot_voice_channel(&self, ctx: &Context, user_id: UserId) -> bool {
        let (bot_ch, user_ch) = self.voice_channels(ctx, user_id);
        voice_gate(bot_ch, user_ch, false)
    }

    /// The Discord user id of the current session owner, if any.
    fn active_owner(&self) -> Option<u64> {
        let lock = self.active_session.lock();
        lock.as_ref().map(|s| s.discord_user_id)
    }

    /// Whether audio is actively being produced right now: a priority
    /// (YouTube/file) item, or the Spotify baseline reported as playing.
    fn something_is_playing(&self) -> bool {
        let priority_active = {
            let lock = self.active_priority_item.lock();
            lock.is_some()
        };
        priority_active || *self.spotify_state.lock() == SpotifyState::Playing
    }

    /// Whether a Spotify Connect session is live (able to accept commands),
    /// regardless of its current playback state.
    fn has_spotify_session(&self) -> bool {
        self.spirc_cmd_tx.lock().is_some()
    }

    /// The queue listing shown by the `ctrl_queue_hint` button and by
    /// `/queue` with no arguments: how to add tracks, the current Spotify
    /// playback state, and the first few priority-queue items.
    fn format_queue_listing(&self) -> String {
        let has_session = self.has_spotify_session();
        let pq_snapshot = {
            let lock = self.priority_queue.lock();
            lock.snapshot()
        };
        let mut lines = vec![];
        if has_session {
            lines.push("Use `/queue <spotify_url>` to add Spotify tracks.".to_string());
        }
        if self.ytdlp_available {
            lines.push("Use `/play <youtube_url>` to add YouTube tracks.".to_string());
        }
        let spotify_line = match *self.spotify_state.lock() {
            SpotifyState::Playing => "Spotify: playing",
            SpotifyState::Paused => "Spotify: paused",
            SpotifyState::Idle => "Spotify: idle",
        };
        lines.push(spotify_line.to_string());
        if !pq_snapshot.is_empty() {
            lines.push(format!("\nPriority queue ({} item(s)):", pq_snapshot.len()));
            for (i, item) in pq_snapshot.iter().enumerate().take(5) {
                let duration = item.source.display_duration()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                lines.push(format!("  {}. {}{} — queued by {}", i + 1, item.source.display_title(), duration, item.queued_by));
            }
        }
        lines.join("\n")
    }

    /// Full playback teardown: abort any Spotify session (deactivating its
    /// owner), stop priority playback, reset presence and controls, and
    /// optionally leave voice. Runs when the voice channel empties and when
    /// the bot is force-disconnected.
    async fn teardown_playback_session(&self, ctx: &Context, leave_voice: bool) {
        let owner = {
            let mut lock = self.active_session.lock();
            lock.take().map(|session| {
                session.abort();
                tracing::info!(user = session.discord_user_id, "aborted session (teardown)");
                session.discord_user_id
            })
        };

        self.stop_priority_playback();

        let _ = self.presence_tx.send(PresenceUpdate::Idle);

        delete_and_repost_controls(ctx, self.text_channel_id, &self.controls_message_id, None).await;

        if leave_voice {
            if let Some(manager) = songbird::get(ctx).await {
                let _ = manager.leave(self.guild_id).await;
                tracing::info!("bot left voice channel");
            }
        }

        // Deactivate only the session owner, not every stored user.
        if let Some(owner) = owner {
            let _ = self.user_store.deactivate(&owner.to_string());
        }
    }

    /// Tear down priority (YouTube/file) playback: cancel any running feeder,
    /// clear the queue, and clear the active item.
    fn stop_priority_playback(&self) {
        let token = {
            let lock = self.feeder_cancel.lock();
            lock.clone()
        };
        if let Some(t) = token {
            t.cancel();
        }
        {
            let mut lock = self.priority_queue.lock();
            lock.clear();
        }
        {
            let mut lock = self.active_priority_item.lock();
            *lock = None;
        }
    }

    /// Whether a user may queue via /play: if the bot is already in a channel,
    /// they must share it (the control rule); if the bot is in no channel yet,
    /// they only need to be in one so the bot can follow them in.
    fn user_can_play(&self, ctx: &Context, user_id: UserId) -> bool {
        let (bot_ch, user_ch) = self.voice_channels(ctx, user_id);
        voice_gate(bot_ch, user_ch, true)
    }

    /// Join the given user's voice channel (falling back to the configured
    /// channel), self-deafen, unsuppress on stage channels, and start the
    /// join-sound + bridge hookup. Returns whether the join succeeded.
    async fn join_voice_for_user(&self, discord_user_id: Option<u64>) -> bool {
        let ctx = {
            let lock = self.ctx.lock();
            match lock.clone() {
                Some(c) => c,
                None => { tracing::warn!("no ctx available for voice join"); return false; }
            }
        };

        let user_channel = discord_user_id.and_then(|id| {
            self.guild_id.to_guild_cached(&ctx)
                .and_then(|guild| {
                    guild.voice_states.get(&UserId::new(id))
                        .and_then(|vs| vs.channel_id)
                })
        });

        let target_channel = user_channel.unwrap_or(self.channel_id);

        let manager = match songbird::get(&ctx).await {
            Some(m) => m,
            None => { tracing::error!("songbird not registered"); return false; }
        };

        match manager.join(self.guild_id, target_channel).await {
            Ok(call) => {
                tracing::info!(channel = %target_channel, "joined voice channel for login");
                // Self-deafen so users know we're not listening
                let bot_id = ctx.cache.current_user().id;
                let _ = self.guild_id.edit_member(&ctx, bot_id,
                    serenity::builder::EditMember::new().deafen(true)).await;
                tracing::info!("self-deafened");
                // On a stage channel the bot joins as a suppressed audience member;
                // unsuppress so its audio is actually heard.
                if let Ok(serenity::all::Channel::Guild(gc)) = target_channel.to_channel(&ctx).await {
                    if gc.kind == serenity::all::ChannelType::Stage {
                        let builder = serenity::builder::EditVoiceState::new().suppress(false);
                        if let Err(e) = gc.edit_own_voice_state(&ctx, builder).await {
                            tracing::warn!(error = ?e, "failed to unsuppress on stage channel");
                        } else {
                            tracing::info!("unsuppressed on stage channel");
                        }
                    }
                }
                let bridge = self.bridge.clone();
                let prebuffer_samples = self.prebuffer_samples;
                let prebuffer_wait = self.prebuffer_wait;
                let track_handle_store = self.track_handle.clone();
                let dj_join = self.dj.clone();
                tokio::spawn(play_join_sound_then_bridge(call, bridge, prebuffer_samples, prebuffer_wait, track_handle_store, dj_join));
                true
            }
            Err(e) => {
                tracing::warn!(error = ?e, "failed to join voice channel");
                false
            }
        }
    }

    /// Restart the stored active user's Spotify session on boot, through the
    /// exact same path /login uses. Skips when no user is marked active or the
    /// stored record is unusable (unparseable id, failed refresh).
    async fn auto_start_stored_session(&self) {
        let oauth = self.oauth.clone();
        let Some(user) = self.user_store.list().into_iter().find(|u| u.active) else {
            tracing::info!("auto-start skipped: no stored active user");
            return;
        };
        let Ok(discord_user_id) = user.discord_user_id.parse::<u64>() else {
            tracing::warn!(user = %user.discord_user_id, "auto-start skipped: unparseable discord user id");
            return;
        };

        tracing::info!(spotify = %user.discord_name, "auto-starting stored session");
        println!("Auto-starting Spotify session for {}...", user.discord_name);

        // A refresh failure at boot means the stored credentials are stale or
        // revoked; retrying with the expired token would just burn reconnect
        // attempts, so skip auto-start and wait for a fresh /login.
        let (access_token, refresh_token, expires_in) =
            match oauth.refresh_access_token(&user.refresh_token).await {
                Ok(t) => {
                    let mut updated = user.clone();
                    updated.access_token = t.access_token.clone();
                    if let Some(rt) = t.refresh_token.clone() {
                        updated.refresh_token = rt;
                    }
                    let _ = self.user_store.save(&updated);
                    (t.access_token, updated.refresh_token, t.expires_in)
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "auto-start token refresh failed; skipping auto-start");
                    // Dead stored token: deactivate it so every boot stops
                    // retrying, and say so in the text channel — a silent
                    // skip looks like the bot lost Spotify support entirely.
                    let _ = self.user_store.deactivate(&user.discord_user_id);
                    let ctx = {
                        let lock = self.ctx.lock();
                        lock.clone()
                    };
                    if let Some(ctx) = ctx {
                        let msg = CreateMessage::new().content(format!(
                            "⚠️ Couldn't restore **{}**'s Spotify session (stored credentials expired). Run `/login` to reconnect.",
                            user.discord_name
                        ));
                        let _ = self.text_channel_id.send_message(&ctx, msg).await;
                    }
                    return;
                }
            };

        self.spawn_session(
            discord_user_id,
            user.discord_name.clone(),
            user.discord_name,
            access_token,
            refresh_token,
            expires_in,
        )
        .await;
    }

    async fn spawn_session(
        &self,
        discord_user_id: u64,
        spotify_name: String,
        discord_name: String,
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    ) {
        // Serialize the whole spawn so two concurrent logins can't both take()
        // the old session and then clobber each other's store, orphaning a task.
        let _spawn_guard = self.spawn_lock.lock().await;
        let generation = self.session_gen.fetch_add(1, Ordering::SeqCst);

        // A fresh Spotify session owns the audio path — cancel any active
        // YouTube/file playback before starting it.
        self.stop_priority_playback();

        let config = self.config.clone();
        let bridge = self.bridge.clone();
        let presence_tx = self.presence_tx.clone();
        let active_session = self.active_session.clone();
        let oauth_for_task = self.oauth.clone();
        let user_store_for_task = self.user_store.clone();
        let user_id_str = discord_user_id.to_string();

        // Shared, single-owner token state. The refresher below is the only
        // writer of the refresh token; the librespot task only reads the
        // current access token and signals the refresher when its session dies.
        let token_state = Arc::new(Mutex::new((access_token.clone(), refresh_token)));
        let refresh_now = Arc::new(Notify::new());

        {
            let mut lock = active_session.lock();
            if let Some(old) = lock.take() {
                tracing::info!(old_user = old.discord_user_id, "aborting existing librespot session");
                old.abort();
            }
        }
        // Exactly one user stays active:true, so auto-start can't resurrect a
        // displaced user after a restart.
        if let Err(e) = self.user_store.set_active_exclusive(&user_id_str) {
            tracing::warn!(error = %e, "failed to set exclusive active user");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = self.join_voice_for_user(Some(discord_user_id)).await;

        {
            let ctx = {
                let lock = self.ctx.lock();
                lock.clone()
            };
            if let Some(ctx) = ctx {
                delete_and_repost_controls(&ctx, self.text_channel_id, &self.controls_message_id, Some(&discord_name)).await;
            }
        }

        // Create channels for priority queue integration
        let (eot_tx, eot_rx) = mpsc::unbounded_channel::<()>();
        let (spirc_tx, spirc_rx) = mpsc::unbounded_channel::<SpircCommand>();

        // Store spirc_cmd_tx
        {
            let mut lock = self.spirc_cmd_tx.lock();
            *lock = Some(spirc_tx.clone());
        }

        // Spawn priority queue manager
        let pq = self.priority_queue.clone();
        let bridge_for_mgr = self.bridge.clone();
        let ctx_for_mgr = self.ctx.clone();
        let text_channel_id = self.text_channel_id;
        let active_priority_item = self.active_priority_item.clone();
        let feeder_cancel = self.feeder_cancel.clone();
        let feeder_paused = self.feeder_paused.clone();
        let controls_message_id = self.controls_message_id.clone();
        let now_playing_message_id = self.now_playing_message_id.clone();

        tokio::spawn(priority_queue_manager(
            eot_rx,
            pq,
            bridge_for_mgr,
            spirc_tx.clone(),
            ctx_for_mgr,
            text_channel_id,
            active_priority_item,
            feeder_cancel,
            feeder_paused,
            self.dj.clone(),
            self.announce_enabled.clone(),
            self.drain_active.clone(),
            controls_message_id,
            now_playing_message_id,
            self.track_handle.clone(),
            self.spotify_state.clone(),
            self.resume_spotify_after_drain.clone(),
        ));

        // Proactive refresher: the sole owner of the refresh cycle. Wakes on a
        // timer (expires_in − margin) or when the librespot task signals its
        // session died, refreshes, and publishes the new access token to the
        // shared state, the DB, and the live ActiveSession.
        let refresh_handle = tokio::spawn({
            let oauth = oauth_for_task.clone();
            let user_store = user_store_for_task.clone();
            let active_session = active_session.clone();
            let token_state = token_state.clone();
            let refresh_now = refresh_now.clone();
            let user_id_str = user_id_str.clone();
            async move {
                let mut lifetime = if expires_in == 0 { DEFAULT_TOKEN_LIFETIME_SECS } else { expires_in };
                loop {
                    let wait = lifetime
                        .saturating_sub(TOKEN_REFRESH_MARGIN_SECS)
                        .max(TOKEN_REFRESH_MIN_WAIT_SECS);
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
                        _ = refresh_now.notified() => {
                            tracing::debug!(user = discord_user_id, "early token refresh requested");
                        }
                    }
                    let current_refresh = { token_state.lock().1.clone() };
                    match oauth.refresh_access_token(&current_refresh).await {
                        Ok(tok) => {
                            let new_refresh = tok.refresh_token.clone().unwrap_or(current_refresh);
                            {
                                let mut s = token_state.lock();
                                s.0 = tok.access_token.clone();
                                s.1 = new_refresh.clone();
                            }
                            if let Some(mut creds) = user_store.load(&user_id_str) {
                                creds.access_token = tok.access_token.clone();
                                creds.refresh_token = new_refresh;
                                let _ = user_store.save(&creds);
                            }
                            {
                                let mut lock = active_session.lock();
                                if let Some(s) = lock.as_mut() {
                                    if s.discord_user_id == discord_user_id {
                                        s.access_token = tok.access_token.clone();
                                    }
                                }
                            }
                            lifetime = if tok.expires_in == 0 { DEFAULT_TOKEN_LIFETIME_SECS } else { tok.expires_in };
                            tracing::info!(user = discord_user_id, lifetime, "access token refreshed");
                        }
                        Err(e) => {
                            tracing::warn!(user = discord_user_id, error = ?e, "token refresh failed; retrying");
                            // Wait out the retry window on the next loop.
                            lifetime = TOKEN_REFRESH_RETRY_SECS + TOKEN_REFRESH_MARGIN_SECS;
                        }
                    }
                }
            }
        });

        let active_session_for_task = active_session.clone();
        let spotify_name_clone = spotify_name.clone();
        let handle = tokio::spawn({
            let token_state = token_state.clone();
            let refresh_now = refresh_now.clone();
            async move {
                tracing::info!(user = discord_user_id, "librespot OAuth session starting");
                let mut spirc_rx = Some(spirc_rx);
                let mut restarts: u32 = 0;
                loop {
                    let access_token = { token_state.lock().0.clone() };
                    let run_start = std::time::Instant::now();
                    match SpotifyPlayer::run_with_token(
                        &config, bridge.clone(), presence_tx.clone(), access_token,
                        Some(eot_tx.clone()),
                        &mut spirc_rx,
                    ).await {
                        Ok(()) => tracing::info!(user = discord_user_id, "librespot session ended cleanly"),
                        Err(e) => tracing::warn!(user = discord_user_id, error = ?e, "librespot session ended with error"),
                    }
                    // Only consecutive *fast* failures count toward giving up; a
                    // session that ran for a while resets the budget.
                    if run_start.elapsed() >= std::time::Duration::from_secs(60) {
                        restarts = 0;
                    } else {
                        restarts += 1;
                    }
                    if restarts >= MAX_SESSION_RESTARTS {
                        tracing::warn!(user = discord_user_id, "librespot session gave up after repeated failures");
                        break;
                    }
                    // Ask the refresher to rotate the token (in case the death was
                    // an auth failure), then retry with whatever it publishes.
                    refresh_now.notify_one();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                // Give-up path: clear the slot only if this exact spawn still
                // owns it, and abort its refresher — dropping the ActiveSession
                // would only detach the refresh task, leaving it rotating the
                // token forever and racing a future /login.
                let owned = {
                    let mut lock = active_session_for_task.lock();
                    match lock.as_ref() {
                        Some(s) if s.generation == generation => lock.take(),
                        _ => None,
                    }
                };
                if let Some(session) = owned {
                    session.abort();
                    let _ = presence_tx.send(PresenceUpdate::Idle);
                }
            }
        });

        let mut lock = active_session.lock();
        let access_token_for_store = { token_state.lock().0.clone() };
        *lock = Some(ActiveSession {
            discord_user_id,
            spotify_name,
            discord_name,
            access_token: access_token_for_store,
            generation,
            handle,
            refresh_handle,
        });
        tracing::info!(user = discord_user_id, spotify = %spotify_name_clone, "librespot session spawned");
    }

    /// Extracts `url`/`file`/`next` from a `/play` or `/queue` interaction's
    /// options. `next` is always `false` for commands without that option
    /// (only `/play` registers it).
    fn parse_play_queue_options(
        cmd: &serenity::model::application::CommandInteraction,
    ) -> (Option<String>, Option<serenity::model::channel::Attachment>, bool) {
        let url_arg: Option<String> = cmd.data.options.iter()
            .find(|o| o.name == "url")
            .and_then(|o| if let serenity::model::application::CommandDataOptionValue::String(s) = &o.value { Some(s.clone()) } else { None });
        let attachment_arg = cmd.data.resolved.attachments.values().next().cloned();
        let next: bool = cmd.data.options.iter()
            .find(|o| o.name == "next")
            .and_then(|o| if let serenity::model::application::CommandDataOptionValue::Boolean(b) = &o.value { Some(*b) } else { None })
            .unwrap_or(false);
        (url_arg, attachment_arg, next)
    }

    /// Builds a `QueueItem` from a YouTube/SoundCloud URL (via yt-dlp
    /// metadata) or a file attachment (via extension/size validation). Not
    /// used for Spotify links, which never enter the priority queue.
    async fn build_media_queue_item(
        url: Option<String>,
        attachment: Option<serenity::model::channel::Attachment>,
        discord_name: &str,
        discord_id: u64,
    ) -> Result<QueueItem, String> {
        if let Some(url) = url {
            let meta = fetch_youtube_metadata(&url).await.map_err(|e| e.to_string())?;
            Ok(QueueItem {
                source: MediaSource::YouTube {
                    url: meta.webpage_url.clone(),
                    video_id: meta.video_id,
                    title: meta.title,
                    channel: meta.channel,
                    thumbnail_url: meta.thumbnail_url,
                    duration_secs: meta.duration_secs,
                },
                queued_by: discord_name.to_string(),
                queued_by_id: discord_id,
            })
        } else {
            let att = attachment.expect("caller ensures url xor attachment is Some");
            validate_attachment(&att.filename, att.size as u64).map_err(|e| e.to_string())?;
            Ok(QueueItem {
                source: MediaSource::File {
                    filename: att.filename.clone(),
                    attachment_url: att.url.clone(),
                },
                queued_by: discord_name.to_string(),
                queued_by_id: discord_id,
            })
        }
    }

    async fn handle_play(
        &self,
        cmd: &serenity::model::application::CommandInteraction,
        ctx: &Context,
    ) {
        let (url_arg, attachment_arg, next) = Self::parse_play_queue_options(cmd);

        if url_arg.is_none() && attachment_arg.is_none() {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ Provide a Spotify/YouTube/SoundCloud URL or attach an audio file.")
                    .ephemeral(true)
            )).await;
            return;
        }
        if url_arg.is_some() && attachment_arg.is_some() {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ Provide either a URL or a file, not both.")
                    .ephemeral(true)
            )).await;
            return;
        }

        let discord_name = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let discord_id = cmd.user.id.get();

        // Spotify track link: no yt-dlp probe, no cooldown — reply
        // immediately instead of deferring.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let reply = if !self.has_spotify_session() {
                    "Run `/login` first.".to_string()
                } else if next {
                    "Spotify tracks always play next after the current track — use `/play` without `next`.".to_string()
                } else {
                    let tx = { let lock = self.spirc_cmd_tx.lock(); lock.clone() };
                    match tx {
                        Some(tx) if self.something_is_playing() => {
                            match tx.send(SpircCommand::AddToQueue(spotify_uri)) {
                                Ok(()) => "Queued on Spotify — plays after the current track.".to_string(),
                                Err(_) => "No active Spotify session.".to_string(),
                            }
                        }
                        Some(tx) => {
                            match tx.send(SpircCommand::Load(spotify_uri)) {
                                Ok(()) => "Playing now on Spotify.".to_string(),
                                Err(_) => "No active Spotify session.".to_string(),
                            }
                        }
                        None => "No active Spotify session.".to_string(),
                    }
                };
                let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(reply).ephemeral(true)
                )).await;
                return;
            }
        }

        if !self.ytdlp_available {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ YouTube playback is not available (yt-dlp not installed).")
                    .ephemeral(true)
            )).await;
            return;
        }

        // Per-user cooldown ahead of the metadata probe: every /play spawns a
        // yt-dlp subprocess before the queue cap applies, so rapid calls
        // would otherwise drive unbounded process pressure.
        const PLAY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);
        let on_cooldown = {
            let now = Instant::now();
            let mut lock = self.play_cooldowns.lock();
            match lock.get(&cmd.user.id.get()) {
                Some(last) if now.duration_since(*last) < PLAY_COOLDOWN => true,
                _ => {
                    lock.insert(cmd.user.id.get(), now);
                    false
                }
            }
        };
        if on_cooldown {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⏳ One /play at a time — try again in a few seconds.")
                    .ephemeral(true)
            )).await;
            return;
        }

        // Defer response
        let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true)
        )).await;

        let queue_item = match Self::build_media_queue_item(url_arg, attachment_arg, &discord_name, discord_id).await {
            Ok(item) => item,
            Err(e) => {
                let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
                    .content(format!("❌ {}", e))
                ).await;
                return;
            }
        };

        let (accepted, queue_len) = {
            let mut lock = self.priority_queue.lock();
            let accepted = if next { lock.push_front(queue_item) } else { lock.push(queue_item) };
            (accepted, lock.len())
        };

        let reply = if !accepted {
            format!("Queue is full ({} items) — try again once some have played.", queue_len)
        } else if self.something_is_playing() {
            // Already playing something — the new item waits its turn and
            // does not start a drain.
            if next { "Playing next".to_string() } else { format!("Added to queue #{}", queue_len) }
        } else {
            // Nothing playing — start a drain immediately (Spotify Connect
            // may still be playing; the drain pauses it).
            match self.trigger_priority_queue_drain(Some(discord_id)).await {
                Err(msg) => format!("❌ {}", msg),
                Ok(_) => "Playing now".to_string(),
            }
        };

        let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
            .content(reply)
        ).await;
    }

    /// Start a queue drain if none is running. Returns Ok(true) when this
    /// call started a drain, Ok(false) when a live drain owns the queue (it
    /// will pick up whatever is queued), and Err with a user-facing message
    /// when the bot could not join voice — nothing would be heard, so the
    /// caller must report the failure instead of claiming playback started.
    async fn trigger_priority_queue_drain(&self, requester_id: Option<u64>) -> Result<bool, String> {
        // One drain at a time. A drain cancelled by skip/next releases the
        // flag within moments, so retry briefly instead of racing it with a
        // fixed sleep. A genuinely live drain keeps the flag for its whole
        // playback and consumes the queue itself — once the queue is empty or
        // the window closes, give up quietly.
        let mut acquired = false;
        for attempt in 0..10 {
            if self.drain_active.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                acquired = true;
                break;
            }
            let queue_empty = {
                let lock = self.priority_queue.lock();
                lock.is_empty()
            };
            if queue_empty || attempt == 9 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if !acquired {
            return Ok(false);
        }
        // Owns the flag from here; drops (releasing it) on every exit path.
        let drain_guard = DrainGuard(self.drain_active.clone());

        // Ensure the bot is in voice before consuming the queue.
        let ctx = {
            let lock = self.ctx.lock();
            lock.clone()
        };
        if let Some(ctx) = &ctx {
            if let Some(manager) = songbird::get(ctx).await {
                if manager.get(self.guild_id).is_none() {
                    if !self.join_voice_for_user(requester_id).await {
                        return Err(
                            "Couldn't join a voice channel, so nothing would be heard. Try again from a voice channel.".to_string()
                        );
                    }
                    // Fixed grace delay for the join sound + bridge hookup
                    // (not a synchronized wait on either).
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }

        let drain_ctx = QueueDrainCtx {
            priority_queue: self.priority_queue.clone(),
            bridge: self.bridge.clone(),
            spirc_cmd_tx: {
                let lock = self.spirc_cmd_tx.lock();
                lock.clone()
            },
            ctx: self.ctx.clone(),
            text_channel_id: self.text_channel_id,
            active_priority_item: self.active_priority_item.clone(),
            feeder_cancel: self.feeder_cancel.clone(),
            feeder_paused: self.feeder_paused.clone(),
            dj: self.dj.clone(),
            announce_enabled: self.announce_enabled.clone(),
            controls_message_id: self.controls_message_id.clone(),
            now_playing_message_id: self.now_playing_message_id.clone(),
            track_handle: self.track_handle.clone(),
            spotify_state: self.spotify_state.clone(),
            resume_spotify_after_drain: self.resume_spotify_after_drain.clone(),
        };
        tokio::spawn(async move {
            // Clears drain_active on any exit (normal, cancel, or abort).
            let _drain_guard = drain_guard;
            run_queue_drain(&drain_ctx).await;
        });
        Ok(true)
    }

    async fn handle_skip(&self) -> String {
        let priority_playing = {
            let lock = self.active_priority_item.lock();
            lock.is_some()
        };

        if priority_playing {
            let token = {
                let lock = self.feeder_cancel.lock();
                lock.clone()
            };
            if let Some(t) = token {
                t.cancel();
            }
            self.bridge.clear();
            // Check if more items exist; if so, start the next one
            let has_more = {
                let lock = self.priority_queue.lock();
                !lock.snapshot().is_empty()
            };
            if has_more {
                // The cancelled drain releases the drain flag within moments;
                // the trigger retries the handoff instead of racing it.
                let _ = self.trigger_priority_queue_drain(None).await;
            } else if self.resume_spotify_after_drain.load(Ordering::SeqCst) {
                // No more items — resume Spotify, but only if it was
                // actually playing before this drain paused it.
                let spirc_tx = {
                    let lock = self.spirc_cmd_tx.lock();
                    lock.clone()
                };
                if let Some(ref tx) = spirc_tx {
                    let _ = tx.send(SpircCommand::Play);
                }
            }
            "⏭ Skipped.".to_string()
        } else {
            let spirc_tx = {
                let lock = self.spirc_cmd_tx.lock();
                lock.clone()
            };
            match spirc_tx {
                Some(tx) => {
                    if tx.send(SpircCommand::Next).is_ok() {
                        "⏭ Skipped.".to_string()
                    } else {
                        "⚠ Spotify didn't accept the skip — try again.".to_string()
                    }
                }
                None => "No active Spotify session.".to_string()
            }
        }
    }

    async fn handle_stop(&self) -> String {
        self.stop_priority_playback();
        self.bridge.clear();

        // /stop means stop: pause Spotify too, never resume it here. Handing
        // playback back to Spotify after an interrupted item is /skip's job.
        let spirc_tx = {
            let lock = self.spirc_cmd_tx.lock();
            lock.clone()
        };
        if let Some(ref tx) = spirc_tx {
            let _ = tx.send(SpircCommand::Pause);
        }

        "⏹ Stopped. Priority queue cleared.".to_string()
    }

    async fn handle_np(&self) -> String {
        let priority_item = {
            let lock = self.active_priority_item.lock();
            lock.clone()
        };
        if let Some(item) = priority_item {
            return format!("🎵 Now playing: **{}** ({})",
                item.source.display_title(),
                item.source.display_subtitle());
        }

        let spotify_name = {
            let lock = self.active_session.lock();
            lock.as_ref().map(|s| s.spotify_name.clone())
        };
        match spotify_name {
            Some(name) => {
                let meta = {
                    let lock = self.last_spotify_meta.lock();
                    lock.clone()
                };
                match meta {
                    Some(m) => {
                        let prefix = if *self.spotify_state.lock() == SpotifyState::Paused { "⏸ Paused" } else { "🎵 Now playing" };
                        format!("{}: **{}** — {} (Spotify session: {})", prefix, m.title, m.artist, name)
                    }
                    None => format!("Spotify session: {} — nothing played yet.", name),
                }
            }
            None => "Nothing is currently playing.".to_string(),
        }
    }

    async fn handle_announce(&self) -> String {
        let current = self.announce_enabled.load(Ordering::Relaxed);
        let new_val = !current;
        self.announce_enabled.store(new_val, Ordering::Relaxed);
        // Persist so restarts (including the VPS updater's) keep the toggle.
        if let Err(e) = self.user_store.set_setting("announce_enabled", if new_val { "1" } else { "0" }) {
            tracing::warn!(error = %e, "failed to persist announce toggle");
        }
        if new_val {
            "🎙️ DJ track announcements **enabled**. Spotibot will announce each track.".to_string()
        } else {
            "🔇 DJ track announcements **disabled**. Greetings still active.".to_string()
        }
    }

    async fn handle_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        in_voice: bool,
    ) -> LoginOutcome {
        // Taking over an active session owned by someone else requires being in
        // the bot's voice channel — you can't evict the current DJ from outside.
        if let Some(owner) = self.active_owner() {
            if owner != user_id_u64 && !in_voice {
                return LoginOutcome::Reply("Someone else is the active DJ. Join the bot's voice channel to take over.".to_string());
            }
        }

        // Stored creds exist: quick re-login by refreshing, no new pairing needed.
        if let Some(existing) = self.user_store.load(user_id) {
            return LoginOutcome::Reply(
                self.reactivate_login(user_id, user_id_u64, discord_username, existing)
                    .await,
            );
        }

        // Fresh login: start a device-code pairing.
        match self.oauth.request_device_code().await {
            Ok(auth) => LoginOutcome::Pair(auth),
            Err(e) => LoginOutcome::Reply(format!("Couldn't start a Spotify login: {e}. Try again.")),
        }
    }

    /// Quick re-login for a user who already authorized once: refresh their
    /// token and (re)start the session without a new browser round-trip.
    async fn reactivate_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        existing: UserCredentials,
    ) -> String {
        match self.oauth.refresh_access_token(&existing.refresh_token).await {
            Ok(new_token) => {
                let expires_in = new_token.expires_in;
                let mut creds = existing.clone();
                creds.active = true;
                creds.access_token = new_token.access_token.clone();
                if let Some(rt) = new_token.refresh_token {
                    creds.refresh_token = rt;
                }
                if let Err(e) = self.user_store.save(&creds) {
                    tracing::error!(error = %e, "failed to save reactivated session");
                    return "Failed to save session. Please try again.".to_string();
                }
                self.spawn_session(
                    user_id_u64,
                    existing.discord_name.clone(),
                    discord_username.to_string(),
                    new_token.access_token,
                    creds.refresh_token.clone(),
                    expires_in,
                )
                .await;
                tracing::info!(user = %user_id, spotify = %existing.discord_name, "session reactivated");
                format!(
                    "Session (re)started for **{}**! Pick **{}** in Spotify's device list to play.",
                    existing.discord_name, self.config.device_name
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "token refresh failed on reactivation; re-authorization required");
                // The stored refresh token is dead. Deactivate it so
                // auto-start stops retrying it, and prompt a fresh
                // authorization instead of dead-ending the user into a
                // /forget + /login round-trip.
                let _ = self.user_store.deactivate(user_id);
                format!(
                    "Your stored Spotify session for **{}** can't be refreshed — run `/login` again to re-authorize.",
                    existing.discord_name
                )
            }
        }
    }

    /// Persist device-flow tokens for this user with the given active flag,
    /// using the Discord display name as the shown Spotify name (the Web API
    /// profile lookup is gone — it 429s under the desktop client ID). Returns
    /// `(display_name, refresh_token)`, or the reply to send when the tokens
    /// can't be stored.
    async fn save_device_creds(
        &self,
        user_id: &str,
        discord_username: &str,
        token: &crate::oauth::TokenResponse,
        active: bool,
    ) -> Result<(String, String), String> {
        let Some(refresh_token) = token.refresh_token.clone() else {
            return Err("Spotify didn't return a refresh token. Run `/login` again.".to_string());
        };
        let display_name = discord_username.to_string();
        let creds = UserCredentials {
            discord_user_id: user_id.to_string(),
            discord_name: discord_username.to_string(),
            spotify_username: display_name.clone(),
            access_token: token.access_token.clone(),
            refresh_token: refresh_token.clone(),
            active,
        };
        if let Err(e) = self.user_store.save(&creds) {
            tracing::error!(error = %e, "failed to save credentials");
            return Err("Failed to save credentials. Please try again.".to_string());
        }
        Ok((display_name, refresh_token))
    }

    /// Poll Spotify for the device-code pairing issued by `handle_login`,
    /// cancellably: a newer `/login` or a `/logout`/`/forget` for this user
    /// notifies the stashed `Notify`, which aborts this poll in place of the
    /// old one.
    async fn finish_device_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        ctx: &Context,
        auth: DeviceAuthorization,
    ) -> String {
        let cancel = Arc::new(Notify::new());
        {
            let mut pending = self.pending_auth.lock();
            if let Some(old) = pending.insert(user_id_u64, cancel.clone()) {
                // A newer /login replaces (and cancels) any prior pairing poll.
                old.notify_one();
            }
        }

        let outcome = tokio::select! {
            r = self.oauth.poll_device_token(&auth, DEVICE_LOGIN_MAX_WAIT) => Some(r),
            _ = cancel.notified() => None,
        };

        {
            let mut pending = self.pending_auth.lock();
            // Only clear our own entry — a newer login may have already
            // replaced it with its own pending pairing.
            if let Some(current) = pending.get(&user_id_u64) {
                if Arc::ptr_eq(current, &cancel) {
                    pending.remove(&user_id_u64);
                }
            }
        }

        let token = match outcome {
            None => return "This login was cancelled by a newer `/login` or a logout.".to_string(),
            Some(Ok(t)) => t,
            Some(Err(crate::oauth::OAuthError::Denied)) => {
                return "Spotify login was declined.".to_string();
            }
            Some(Err(crate::oauth::OAuthError::Expired)) => {
                return "That code expired. Run `/login` again.".to_string();
            }
            Some(Err(e)) => {
                return format!("Spotify login failed: {e}. Run `/login` again.");
            }
        };

        // Taking over an active session owned by someone else requires being in
        // the bot's voice channel — you can't evict the current DJ from
        // outside. Re-checked here since the poll can take minutes. The
        // tokens are stored inactive so the retry is a quick re-login and the
        // current DJ's row stays the only active one.
        if let Some(owner) = self.active_owner() {
            if owner != user_id_u64 && !self.user_in_bot_voice_channel(ctx, UserId::new(user_id_u64)) {
                return match self.save_device_creds(user_id, discord_username, &token, false).await {
                    Ok(_) => "Saved your Spotify login. Join the bot's voice channel and run `/login` again to take over.".to_string(),
                    Err(msg) => msg,
                };
            }
        }

        let (display_name, refresh_token) =
            match self.save_device_creds(user_id, discord_username, &token, true).await {
                Ok(v) => v,
                Err(msg) => return msg,
            };
        tracing::info!(user = %user_id, spotify = %display_name, "device login successful");
        self.spawn_session(
            user_id_u64,
            display_name.clone(),
            discord_username.to_string(),
            token.access_token,
            refresh_token,
            token.expires_in,
        )
        .await;
        format!(
            "Logged in as **{display_name}**! Spotify session started.\n\
             Open Spotify on any device, tap the Connect (devices) icon, and pick \
             **{}** — it appears from anywhere, no shared network needed.",
            self.config.device_name
        )
    }

    async fn handle_logout(&self, user_id: &str, user_id_u64: u64) -> String {
        // A pending device-code pairing for this user is now moot — cancel its poll.
        if let Some(cancel) = self.pending_auth.lock().remove(&user_id_u64) {
            cancel.notify_one();
        }

        // Serialize against spawn_session so a logout landing in the spawn
        // window can't miss the not-yet-stored session (and then deactivate the
        // DB row while a live session keeps running).
        let _spawn_guard = self.spawn_lock.lock().await;

        // Only the owner of the live session may tear it down. A bystander's
        // /logout must not pause the DJ's audio or wipe the controls.
        let owned_live_session = {
            let mut lock = self.active_session.lock();
            match lock.as_ref() {
                Some(session) if session.discord_user_id == user_id_u64 => {
                    session.abort();
                    *lock = None;
                    tracing::info!(user = %user_id, "active librespot session aborted");
                    true
                }
                _ => false,
            }
        };

        if owned_live_session {
            // Also tear down any priority (YouTube/file) playback, mirroring the
            // empty-channel path — otherwise a queued track keeps playing and
            // posts a history embed after logout.
            self.stop_priority_playback();
            let _ = self.presence_tx.send(PresenceUpdate::Idle);
            let ctx = {
                let lock = self.ctx.lock();
                lock.clone()
            };
            if let Some(ctx) = ctx {
                delete_and_repost_controls(&ctx, self.text_channel_id, &self.controls_message_id, None).await;
            }
        }

        match self.user_store.deactivate(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "session deactivated"); "Session deactivated. Your credentials are kept — run `/login` to reactivate without re-authorizing.".to_string() }
            Ok(false) if owned_live_session => "Session stopped.".to_string(),
            Ok(false) => "You don't have an active session.".to_string(),
            Err(e) => { tracing::error!("failed to deactivate session: {}", e); "Failed to deactivate session.".to_string() }
        }
    }

    async fn handle_forget(&self, user_id: &str, user_id_u64: u64) -> String {
        // A pending device-code pairing for this user is now moot — cancel its poll.
        if let Some(cancel) = self.pending_auth.lock().remove(&user_id_u64) {
            cancel.notify_one();
        }

        match self.user_store.remove(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "credentials forgotten"); "Credentials permanently deleted. Run `/login` to connect again.".to_string() }
            Ok(false) => "No stored credentials to delete.".to_string(),
            Err(e) => { tracing::error!("failed to delete credentials: {}", e); "Failed to delete credentials.".to_string() }
        }
    }

    async fn handle_who(&self) -> String {
        let lock = self.active_session.lock();
        match lock.as_ref() {
            Some(session) => format!("Active session: **{}** (Discord: {})", session.spotify_name, session.discord_name),
            None => "No active Spotify session. Run `/login` to connect.".to_string(),
        }
    }

    /// `/queue`: adds to the queue without starting playback. A Spotify
    /// track link goes to the Spotify queue directly (bypassing the
    /// priority queue); a YouTube/SoundCloud URL or attachment is pushed
    /// onto the priority queue's tail — never jumps the line, never starts
    /// a drain. No arguments shows the current queue listing.
    async fn handle_queue(
        &self,
        cmd: &serenity::model::application::CommandInteraction,
        ctx: &Context,
    ) {
        let (url_arg, attachment_arg, _next) = Self::parse_play_queue_options(cmd);

        if url_arg.is_none() && attachment_arg.is_none() {
            let content = self.format_queue_listing();
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().content(content).ephemeral(true)
            )).await;
            return;
        }
        if url_arg.is_some() && attachment_arg.is_some() {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ Provide either a URL or a file, not both.")
                    .ephemeral(true)
            )).await;
            return;
        }

        // Spotify track link: straight to the Spotify queue, no defer needed.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let reply = if !self.has_spotify_session() {
                    "Run `/login` first.".to_string()
                } else {
                    let tx = { let lock = self.spirc_cmd_tx.lock(); lock.clone() };
                    match tx {
                        Some(tx) => match tx.send(SpircCommand::AddToQueue(spotify_uri)) {
                            Ok(()) => {
                                let mut msg = "Queued on Spotify.".to_string();
                                if *self.spotify_state.lock() == SpotifyState::Idle {
                                    msg.push_str(" Nothing is playing — press ▶ or use `/play` to start.");
                                }
                                msg
                            }
                            Err(_) => "No active Spotify session.".to_string(),
                        },
                        None => "No active Spotify session.".to_string(),
                    }
                };
                let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(reply).ephemeral(true)
                )).await;
                return;
            }
        }

        // YouTube/SoundCloud URL, or a file attachment: goes on the priority
        // queue's tail via the same metadata probe /play uses.
        if url_arg.is_some() && !self.ytdlp_available {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ YouTube playback is not available (yt-dlp not installed).")
                    .ephemeral(true)
            )).await;
            return;
        }

        let discord_name = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let discord_id = cmd.user.id.get();

        let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true)
        )).await;

        let queue_item = match Self::build_media_queue_item(url_arg, attachment_arg, &discord_name, discord_id).await {
            Ok(item) => item,
            Err(e) => {
                let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
                    .content(format!("❌ {}", e))
                ).await;
                return;
            }
        };

        let was_empty = {
            let lock = self.priority_queue.lock();
            lock.is_empty()
        };
        let (accepted, queue_len) = {
            let mut lock = self.priority_queue.lock();
            let accepted = lock.push(queue_item);
            (accepted, lock.len())
        };

        let reply = if !accepted {
            format!("Queue is full ({} items) — try again once some have played.", queue_len)
        } else if !self.something_is_playing() && was_empty {
            "Queued. Nothing is playing right now — press ▶ or use `/play` to start.".to_string()
        } else {
            format!("Added to queue #{}", queue_len)
        };

        let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
            .content(reply)
        ).await;
    }
}

pub struct DiscordBot {
    client: Client,
    ready_rx: mpsc::Receiver<ReadySignal>,
}

impl DiscordBot {
    pub async fn new(
        config: Arc<Config>,
        bridge: Arc<AudioBridge>,
        presence_rx: mpsc::UnboundedReceiver<PresenceUpdate>,
        presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
        user_store: Arc<UserStore>,
        oauth: Arc<SpotifyOAuth>,
        ytdlp_available: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_VOICE_STATES
            | GatewayIntents::GUILD_MEMBERS;

        let (ready_tx, ready_rx) = mpsc::channel(1);

        // The prebuffer target must fit inside the bridge, or the prebuffer
        // loop can never reach it and every playback start burns the full
        // wait against a saturated, dropping buffer.
        let bridge_capacity =
            SAMPLE_RATE as usize * CHANNELS as usize * config.audio_buffer_seconds;
        let prebuffer_target =
            (config.prebuffer_seconds * SAMPLE_RATE as f32) as usize * CHANNELS as usize;
        let prebuffer_samples = prebuffer_target.min(bridge_capacity);
        if prebuffer_target > bridge_capacity {
            tracing::warn!(
                prebuffer_seconds = config.prebuffer_seconds,
                audio_buffer_seconds = config.audio_buffer_seconds,
                "PREBUFFER_SECONDS exceeds AUDIO_BUFFER_SECONDS — clamping prebuffer to the bridge capacity"
            );
        }
        let prebuffer_wait =
            std::time::Duration::from_secs_f32((config.prebuffer_seconds + 0.5).clamp(0.0, 5.0));

        let active_session = Arc::new(Mutex::new(None::<ActiveSession>));
        let track_handle: Arc<Mutex<Option<TrackHandle>>> = Arc::new(Mutex::new(None));

        let dj = Arc::new(DJAnnouncer::new());
        let announce_persisted = user_store.get_setting("announce_enabled");
        let handler = Handler {
            guild_id: GuildId::new(config.discord_guild_id),
            channel_id: ChannelId::new(config.discord_channel_id),
            text_channel_id: ChannelId::new(config.discord_text_channel_id),
            bridge,
            config: config.clone(),
            ready_tx,
            presence_rx: Mutex::new(Some(presence_rx)),
            presence_tx,
            prebuffer_samples,
            prebuffer_wait,
            user_store,
            oauth,
            pending_auth: Arc::new(Mutex::new(HashMap::new())),
            active_session,
            spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
            session_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            track_handle,
            ctx: Arc::new(Mutex::new(None)),
            controls_message_id: Arc::new(Mutex::new(None)),
            now_playing_message_id: Arc::new(Mutex::new(None)),
            // YouTube/file fields
            ytdlp_available,
            priority_queue: Arc::new(Mutex::new(PriorityQueue::new())),
            spirc_cmd_tx: Arc::new(Mutex::new(None)),
            active_priority_item: Arc::new(Mutex::new(None)),
            drain_active: Arc::new(AtomicBool::new(false)),
            feeder_cancel: Arc::new(Mutex::new(None)),
            feeder_paused: Arc::new(AtomicBool::new(false)),
            dj,
            // Restore the persisted /announce toggle so restarts (including
            // the VPS updater's) don't silently disable announcements.
            announce_enabled: Arc::new(AtomicBool::new(
                announce_persisted.as_deref() == Some("1"),
            )),
            last_spotify_meta: Arc::new(Mutex::new(None)),
            spotify_state: Arc::new(Mutex::new(SpotifyState::Idle)),
            resume_spotify_after_drain: Arc::new(AtomicBool::new(false)),
            play_cooldowns: Arc::new(Mutex::new(HashMap::new())),
            auto_start_attempted: AtomicBool::new(false),
        };

        let token = config.discord_token.clone();
        let client = Client::builder(&token, intents)
            .event_handler(handler)
            .register_songbird()
            .await?;

        Ok(Self { client, ready_rx })
    }

    pub async fn start_background(
        mut self,
    ) -> Result<mpsc::Receiver<ReadySignal>, Box<dyn std::error::Error + Send + Sync>> {
        tokio::spawn(async move {
            if let Err(e) = self.client.start().await {
                tracing::error!(error = ?e, "discord client error");
            }
        });
        Ok(self.ready_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_valid_track_id, parse_track_id_from_url, voice_gate,
        DrainGuard,
    };
    use serenity::all::ChannelId;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const ID: &str = "4cOdK2wGLETKBW3PvgPWqT"; // 22 base62 chars

    // --- voice_gate: the authorization rule behind every playback command ---

    #[test]
    fn gate_requires_sharing_the_bots_channel() {
        let bot = Some(ChannelId::new(10));
        assert!(voice_gate(bot, Some(ChannelId::new(10)), false), "same channel passes");
        assert!(!voice_gate(bot, Some(ChannelId::new(11)), false), "other channel fails");
        assert!(!voice_gate(bot, None, false), "not in voice fails");
        // allow_follow changes nothing while the bot IS in a channel.
        assert!(!voice_gate(bot, Some(ChannelId::new(11)), true));
        assert!(!voice_gate(bot, None, true));
    }

    #[test]
    fn gate_with_bot_out_of_voice_depends_on_follow() {
        // Strict commands (buttons, /skip, /stop…) fail when the bot isn't in
        // voice; /play's follow mode only needs the requester to be in one.
        assert!(!voice_gate(None, Some(ChannelId::new(10)), false));
        assert!(voice_gate(None, Some(ChannelId::new(10)), true));
        assert!(!voice_gate(None, None, true), "follow still needs the user in voice");
    }

    // --- DrainGuard: the single-flight drain flag ---

    #[test]
    fn drain_flag_is_single_flight_and_guard_releases_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok());
        let guard = DrainGuard(flag.clone());
        // A second would-be drain loses the race while the first one runs.
        assert!(flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err());
        drop(guard);
        assert!(!flag.load(Ordering::SeqCst), "drop released the flag");
        assert!(flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok());
    }

    #[test]
    fn drain_guard_releases_even_on_panic() {
        // The abort-safety property the guard exists for: a drain that panics
        // (or is cancelled) must not wedge every future drain.
        let flag = Arc::new(AtomicBool::new(true));
        let flag2 = flag.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = DrainGuard(flag2);
            panic!("drain blew up");
        });
        assert!(result.is_err());
        assert!(!flag.load(Ordering::SeqCst), "flag released during unwind");
    }

    #[test]
    fn parses_plain_url() {
        assert_eq!(
            parse_track_id_from_url(&format!("https://open.spotify.com/track/{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn parses_url_with_si_query() {
        assert_eq!(
            parse_track_id_from_url(&format!("https://open.spotify.com/track/{ID}?si=abc123")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn parses_locale_prefixed_url() {
        assert_eq!(
            parse_track_id_from_url(&format!("https://open.spotify.com/intl-fr/track/{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn parses_uri() {
        assert_eq!(
            parse_track_id_from_url(&format!("spotify:track:{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn rejects_query_param_injection() {
        // A crafted id with an extra param must not survive validation, or it
        // would ride into the authenticated queue POST's query string.
        assert!(parse_track_id_from_url(&format!("spotify:track:{ID}&device_id=x")).is_none());
        assert!(parse_track_id_from_url("spotify:track:abc&foo=bar").is_none());
    }

    #[test]
    fn rejects_wrong_length_and_nonalnum() {
        assert!(!is_valid_track_id("too-short"));
        assert!(!is_valid_track_id(&"x".repeat(23)));
        assert!(!is_valid_track_id("4cOdK2wGLETKBW3PvgPWq!")); // 22 chars, bad byte
        assert!(is_valid_track_id(ID));
    }

    #[test]
    fn rejects_unrelated_input() {
        assert!(parse_track_id_from_url("https://youtube.com/watch?v=abc").is_none());
        assert!(parse_track_id_from_url("just some text").is_none());
    }
}
