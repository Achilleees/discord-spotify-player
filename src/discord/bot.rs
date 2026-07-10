use super::presence::run_presence_loop;
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio::generate_join_sound;
use crate::audio::dj::DJAnnouncer;
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::{new_pkce, parse_redirect, PkceChallenge, SpotifyOAuth};
use crate::presence::PresenceUpdate;
use crate::queue::{PriorityQueue, QueueItem, MediaSource};
use crate::spotify::metadata::{fetch_track_metadata, TrackMetadata};
use crate::spotify::SpotifyPlayer;
use crate::spotify::SpircCommand;
use crate::youtube::metadata::{fetch_youtube_metadata, validate_attachment};
// P0/P1 imports used below
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
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type ReadySignal = Result<(), String>;

pub struct ActiveSession {
    pub discord_user_id: u64,
    pub spotify_name: String,
    pub discord_name: String,
    pub access_token: String,
    pub handle: JoinHandle<()>,
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
    /// Pending PKCE challenges keyed by Discord user, awaiting paste-back.
    pending_auth: Arc<Mutex<HashMap<u64, (PkceChallenge, Instant)>>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
    ctx: Arc<Mutex<Option<Context>>>,
    controls_message_id: Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: Arc<Mutex<Option<MessageId>>>,
    // YouTube/file playback fields
    ytdlp_available: bool,
    priority_queue: Arc<Mutex<PriorityQueue>>,
    spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    feeder_cancel: Arc<Mutex<Option<CancellationToken>>>,
    feeder_paused: Arc<AtomicBool>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    auto_start_attempted: AtomicBool,
}



fn register_commands(ytdlp_available: bool) -> Vec<CreateCommand> {
    let mut cmds = vec![
        CreateCommand::new("login")
            .description("Connect your Spotify account (or reactivate existing session)")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "code",
                    "Paste the redirect URL (or code) after authorizing",
                )
                .required(false),
            ),
        CreateCommand::new("logout")
            .description("Deactivate your Spotify session (credentials kept for quick re-login)"),
        CreateCommand::new("forget")
            .description("Permanently delete your stored Spotify credentials"),
        CreateCommand::new("who")
            .description("Show whose Spotify account is currently active"),
        CreateCommand::new("queue")
            .description("Add a track to the Spotify queue")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "url",
                    "Spotify track URL or URI (e.g. https://open.spotify.com/track/... or spotify:track:...)",
                )
                .required(true),
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
                .description("Play a YouTube URL or file attachment")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "url",
                        "YouTube URL (or any yt-dlp supported URL)")
                    .required(false),
                )
                .add_option(
                    CreateCommandOption::new(CommandOptionType::Attachment, "file",
                        "Audio file to play (mp3, flac, ogg, wav, m4a, aac, opus)")
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
    let mut lock = track_handle_store.lock().unwrap_or_else(|e| e.into_inner());
    *lock = Some(track_handle);
}

// --- Embed builders ---

fn build_now_playing_embed(meta: &TrackMetadata, spotify_name: &str) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(0x1DB954u32)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(format!("{} — {}", meta.title, meta.artist))
        .url(format!("https://open.spotify.com/track/{}", meta.spotify_track_id))
        .timestamp(Timestamp::now());

    if !spotify_name.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(format!("🎧 {}", spotify_name)));
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.image(art_url);
    }

    embed
}

fn build_history_embed(meta: &TrackMetadata, spotify_name: &str) -> CreateEmbed {
    let footer_text = if spotify_name.is_empty() {
        String::new()
    } else {
        format!("played by {}", spotify_name)
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(format!(
            "[{} — {}](https://open.spotify.com/track/{})",
            meta.title, meta.artist, meta.spotify_track_id
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

    let mut embed = CreateEmbed::new()
        .color(color)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(format!("{} — {}", title, subtitle))
        .timestamp(Timestamp::now())
        .footer(CreateEmbedFooter::new(format!("{} {}", footer_icon, item.queued_by)));

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
    let footer_text = format!("played by {}", item.queued_by);
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

fn build_controls_embed(active_user: Option<&str>, waiting: bool) -> CreateEmbed {
    match active_user {
        Some(name) if waiting => CreateEmbed::new()
            .color(0x1DB954u32)
            .title(format!("🎛️ {}", name))
            .description("*Play something to get started!*"),
        Some(name) => CreateEmbed::new()
            .color(0x1DB954u32)
            .title(format!("🎛️ {} is playing", name)),
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
    let embed = build_controls_embed(active_user, active_user.is_some());
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
        let lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
        *lock
    };
    if let Some(mid) = old_id {
        let _ = text_channel_id.delete_message(ctx, mid).await;
    }

    let new_id = post_controls(ctx, text_channel_id, active_user).await;
    let mut lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
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

async fn spotify_playback_command(access_token: &str, method: &str, endpoint: &str) {
    let client = reqwest::Client::new();
    let url = format!("https://api.spotify.com/v1/me/player/{}", endpoint);
    let req = match method {
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        _ => return,
    };
    match req
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Length", "0")
        .send()
        .await
    {
        Ok(r) => tracing::info!(status = r.status().as_u16(), endpoint, "spotify API call"),
        Err(e) => tracing::warn!(error = ?e, endpoint, "spotify API call failed"),
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
        let lock = ctx_store.lock().unwrap_or_else(|e| e.into_inner());
        match lock.clone() { Some(c) => c, None => return }
    };

    // Delete previous now-playing
    let prev_np = {
        let lock = now_playing_message_id.lock().unwrap_or_else(|e| e.into_inner());
        *lock
    };
    if let Some(mid) = prev_np {
        let _ = text_channel_id.delete_message(&ctx, mid).await;
    }

    // Delete old controls
    let old_ctrl = {
        let lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut np_lock = now_playing_message_id.lock().unwrap_or_else(|e| e.into_inner());
            *np_lock = Some(m.id);
            let mut ctrl_lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
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
        let lock = ctx_store.lock().unwrap_or_else(|e| e.into_inner());
        match lock.clone() { Some(c) => c, None => return }
    };

    let embed = build_priority_history_embed(item);
    let msg = CreateMessage::new().embed(embed);
    let _ = text_channel_id.send_message(&ctx, msg).await;
}

// --- Priority queue manager ---

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
    controls_message_id: Arc<Mutex<Option<MessageId>>>,
    now_playing_message_id: Arc<Mutex<Option<MessageId>>>,
) {
    loop {
        match end_of_track_rx.recv().await {
            Some(()) => {}
            None => {
                tracing::debug!("priority queue manager: channel closed, exiting");
                return;
            }
        }

        // Drain the priority queue
        loop {
            let item = {
                let mut lock = priority_queue.lock().unwrap_or_else(|e| e.into_inner());
                lock.pop()
            };
            let item = match item {
                Some(i) => i,
                None => break,
            };

            // Pause Spotify
            let _ = spirc_cmd_tx.send(SpircCommand::Pause);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            bridge.clear();

            // Store current item
            {
                let mut lock = active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                *lock = Some(item.clone());
            }

            // Post now-playing embed
            post_priority_now_playing(
                &ctx, text_channel_id, &item,
                &controls_message_id, &now_playing_message_id,
            ).await;

            // DJ announcement before track
            if dj.is_enabled() {
                let title = item.source.display_title().to_string();
                let subtitle = item.source.display_subtitle().to_string();
                let queued_by = item.queued_by.clone();
                if let Some(clip) = dj.track_announce_clip(&title, &subtitle, &queued_by).await {
                    bridge.push_overlay(&clip);
                }
            }
            // Create cancel token
            let token = CancellationToken::new();
            {
                let mut lock = feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                *lock = Some(token.clone());
            }
            feeder_paused.store(false, Ordering::Relaxed);

            // Run the feeder
            let feed_result = match &item.source {
                MediaSource::YouTube { url, .. } => {
                    feed_youtube_to_bridge(url, bridge.clone(), token, feeder_paused.clone()).await
                }
                MediaSource::File { attachment_url, filename, .. } => {
                    let ext = filename.rsplit('.').next().unwrap_or("mp3");
                    feed_file_to_bridge(attachment_url, ext, bridge.clone(), token, feeder_paused.clone()).await
                }
            };

            match feed_result {
                Ok(()) => {
                    tracing::info!("priority item finished: {}", item.source.display_title());
                }
                Err(FeederError::Cancelled) => {
                    tracing::info!("priority item cancelled (skip/stop)");
                    let mut lock = active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                    *lock = None;
                    // Don't resume Spotify here — let the skip/stop handler decide
                    break;
                }
                Err(e) => {
                    tracing::warn!("feeder error: {}", e);
                }
            }

            // Post history embed
            post_priority_history(&ctx, text_channel_id, &item).await;

            // Clear current item
            {
                let mut lock = active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                *lock = None;
            }
        }

        // Priority queue drained — resume Spotify
        let _ = spirc_cmd_tx.send(SpircCommand::Play);
    }
}

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
) {
    let (fwd_tx, fwd_rx) = mpsc::unbounded_channel::<PresenceUpdate>();
    let ctx_presence = ctx.clone();
    tokio::spawn(async move {
        run_presence_loop(ctx_presence, fwd_rx).await;
    });

    let mut last_track_key: Option<String> = None;
    let mut last_meta: Option<TrackMetadata> = None;
    let mut last_spotify_name: String = String::new();
    let mut is_paused: bool = false;

    while let Some(update) = rx.recv().await {
        let _ = fwd_tx.send(update.clone());

        {
            let lock = track_handle_store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(handle) = lock.as_ref() {
                match &update {
                    PresenceUpdate::Playing { .. } => { let _ = handle.play(); }
                    PresenceUpdate::Paused | PresenceUpdate::Idle => { let _ = handle.pause(); }
                }
            }
        }

        let was_paused = is_paused;
        match &update {
            PresenceUpdate::Paused => { is_paused = true; }
            PresenceUpdate::Playing { .. } => { is_paused = false; }
            _ => {}
        }
        if was_paused != is_paused {
            let msg_id = {
                let lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
                *lock
            };
            if let Some(mid) = msg_id {
                let buttons = build_controls_buttons(is_paused);
                let edit = EditMessage::new().components(vec![buttons]);
                let _ = text_channel_id.edit_message(&ctx, mid, edit).await;
            }
        }

        if let PresenceUpdate::Playing { title, artist, track_id, access_token } = &update {
            let track_key = format!("{} — {}", title, artist);
            if last_track_key.as_deref() != Some(&track_key) {
                last_track_key = Some(track_key.clone());

                let spotify_name = {
                    let lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().map(|s| s.discord_name.clone()).unwrap_or_default()
                };

                let prev_msg_id = {
                    let lock = now_playing_message_id.lock().unwrap_or_else(|e| e.into_inner());
                    *lock
                };
                if let Some(mid) = prev_msg_id {
                    let _ = text_channel_id.delete_message(&ctx, mid).await;
                    if let Some(ref meta) = last_meta {
                        let history_embed = build_history_embed(meta, &last_spotify_name);
                        let msg = CreateMessage::new().embed(history_embed);
                        let _ = text_channel_id.send_message(&ctx, msg).await;
                    }
                }

                let meta = if !track_id.is_empty() && !access_token.is_empty() {
                    fetch_track_metadata(track_id, access_token).await
                } else {
                    None
                };

                let meta = meta.unwrap_or(TrackMetadata {
                    title: title.clone(),
                    artist: artist.clone(),
                    album_art_url: None,
                    spotify_track_id: track_id.clone(),
                });

                let embed = build_now_playing_embed(&meta, &spotify_name);
                let buttons = build_controls_buttons(false);
                let msg = CreateMessage::new().embed(embed).components(vec![buttons]);

                {
                    let old_ctrl = {
                        let lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
                        *lock
                    };
                    if let Some(mid) = old_ctrl {
                        let _ = text_channel_id.delete_message(&ctx, mid).await;
                    }
                }

                match text_channel_id.send_message(&ctx, msg).await {
                    Ok(m) => {
                        tracing::info!(title = %title, artist = %artist, "now-playing embed sent");
                        let mut lock = now_playing_message_id.lock().unwrap_or_else(|e| e.into_inner());
                        *lock = Some(m.id);
                        let mut ctrl_lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
                        *ctrl_lock = Some(m.id);
                    }
                    Err(e) => tracing::warn!(error = ?e, "failed to send now-playing embed"),
                }

                last_meta = Some(meta);
                last_spotify_name = spotify_name;

                // DJ announcement AFTER embed (non-blocking, only if enabled)
                if announce_enabled.load(Ordering::Relaxed) {
                let dj_title = title.clone();
                let dj_artist = artist.clone();
                let dj_ref = dj.clone();
                let bridge_ref = bridge.clone();
                tokio::spawn(async move {
                    let _ = std::fs::write("/opt/openclaw/services/spotibot/debug_reached.txt",
                        format!("reached: {} - {}", dj_title, dj_artist));
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
        } else if matches!(update, PresenceUpdate::Idle) {
            last_track_key = None;
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
            if msg.author.id == bot_id && !msg.embeds.is_empty() {
                if msg.embeds.iter().any(|e| e.title.as_deref() == Some("🎛️ Spotibot")) {
                    let _ = text_channel_id.delete_message(ctx, msg.id).await;
                }
            }
        }
    }

    let new_id = post_controls(ctx, text_channel_id, None).await;
    let mut lock = controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
    *lock = new_id;
}

pub fn check_ytdlp_available() -> bool {
    std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn check_ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
            let mut ctx_store = self.ctx.lock().unwrap_or_else(|e| e.into_inner());
            *ctx_store = Some(ctx.clone());
        }
        let _ = self.ready_tx.send(Ok(())).await;

        let ctx_for_controls = ctx.clone();
        let text_channel_id = self.text_channel_id;
        let bot_id = ready.user.id;
        let controls_id = self.controls_message_id.clone();
        tokio::spawn(async move {
            startup_controls(&ctx_for_controls, text_channel_id, bot_id, &controls_id).await;
        });

        let rx_taken = {
            let mut presence_rx = self.presence_rx.lock().unwrap_or_else(|e| e.into_inner());
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
            tokio::spawn(async move {
                run_presence_loop_with_track(
                    ctx_presence, rx, track_handle_store, active_session,
                    text_channel_id, controls_id, np_id,
                    dj_presence, announce_presence,
                    bridge_presence,
                ).await;
            });
        }

        // Auto-start: replay the stored active user's session through the same
        // machinery /login uses (voice join, controls, priority queue, refresh
        // loop). Guarded so gateway reconnects can't spawn a second session.
        if !self.auto_start_attempted.swap(true, Ordering::SeqCst) {
            self.auto_start_stored_session().await;
        }
    }

    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        if new.guild_id != Some(self.guild_id) {
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

        if humans_in_bot_channel == 0 {
            let has_session = {
                let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                lock.is_some()
            };

            if has_session {
                tracing::info!("voice channel empty — auto-logout triggered");

                {
                    let mut lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(session) = lock.take() {
                        session.handle.abort();
                        tracing::info!(user = session.discord_user_id, "auto-aborted session (empty channel)");
                    }
                }

                // Cancel any active feeder
                {
                    let token = {
                        let lock = self.feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                        lock.clone()
                    };
                    if let Some(t) = token { t.cancel(); }
                }
                // Clear priority queue
                {
                    let mut lock = self.priority_queue.lock().unwrap_or_else(|e| e.into_inner());
                    lock.clear();
                }
                {
                    let mut lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                    *lock = None;
                }

                let _ = self.presence_tx.send(PresenceUpdate::Idle);

                delete_and_repost_controls(&ctx, self.text_channel_id, &self.controls_message_id, None).await;

                if let Some(manager) = songbird::get(&ctx).await {
                    let _ = manager.leave(self.guild_id).await;
                    tracing::info!("bot left voice channel (channel empty)");
                }

                for user in self.user_store.list() {
                    if user.active {
                        let _ = self.user_store.deactivate(&user.discord_user_id);
                    }
                }
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        tracing::info!("interaction_create fired");

        if let Interaction::Component(component) = &interaction {
            let custom_id = component.data.custom_id.as_str();
            tracing::info!(custom_id, "button interaction received");

            let access_token = {
                let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                lock.as_ref().map(|s| s.access_token.clone())
            };

            let priority_playing = {
                let lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                lock.is_some()
            };

            let _reply_content = match custom_id {
                "ctrl_prev" => {
                    if let Some(token) = &access_token {
                        spotify_playback_command(token, "POST", "previous").await;
                    }
                    "⏮ Previous"
                }
                "ctrl_next" => {
                    if priority_playing {
                        let token = {
                            let lock = self.feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                            lock.clone()
                        };
                        if let Some(t) = token { t.cancel(); }
                        self.bridge.clear();
                        "⏭ Skipped"
                    } else if let Some(token) = &access_token {
                        spotify_playback_command(token, "POST", "next").await;
                        "⏭ Skipped"
                    } else {
                        "No active session"
                    }
                }
                "ctrl_pause_toggle" => {
                    if priority_playing {
                        let current = self.feeder_paused.load(Ordering::Relaxed);
                        let new_paused = !current;
                        self.feeder_paused.store(new_paused, Ordering::Relaxed);
                        // P1: Update button visual to reflect pause/play state
                        let msg_id = {
                            let lock = self.controls_message_id.lock().unwrap_or_else(|e| e.into_inner());
                            *lock
                        };
                        if let Some(mid) = msg_id {
                            let buttons = build_controls_buttons(new_paused);
                            let edit = EditMessage::new().components(vec![buttons]);
                            let _ = self.text_channel_id.edit_message(&ctx, mid, edit).await;
                        }
                        if current { "▶ Resumed" } else { "⏸ Paused" }
                    } else if let Some(token) = &access_token {
                        let handle_clone = {
                            let lock = self.track_handle.lock().unwrap_or_else(|e| e.into_inner());
                            lock.as_ref().cloned()
                        };
                        let is_paused = if let Some(h) = handle_clone {
                            h.get_info().await
                                .map(|info| info.playing == songbird::tracks::PlayMode::Pause)
                                .unwrap_or(false)
                        } else {
                            false
                        };
                        if is_paused {
                            spotify_playback_command(token, "PUT", "play").await;
                            "▶ Resumed"
                        } else {
                            spotify_playback_command(token, "PUT", "pause").await;
                            "⏸ Paused"
                        }
                    } else {
                        "No active session"
                    }
                }
                "ctrl_queue_hint" => {
                    let pq_snapshot = {
                        let lock = self.priority_queue.lock().unwrap_or_else(|e| e.into_inner());
                        lock.snapshot()
                    };
                    let mut lines = vec![];
                    if access_token.is_some() {
                        lines.push("Use `/queue <spotify_url>` to add Spotify tracks.".to_string());
                    }
                    if self.ytdlp_available {
                        lines.push("Use `/play <youtube_url>` to add YouTube tracks.".to_string());
                    }
                    if !pq_snapshot.is_empty() {
                        lines.push(format!("\nPriority queue ({} item(s)):", pq_snapshot.len()));
                        for (i, item) in pq_snapshot.iter().enumerate().take(5) {
                            lines.push(format!("  {}. {} — queued by {}", i + 1, item.source.display_title(), item.queued_by));
                        }
                    }
                    let content = if lines.is_empty() { "Nothing in queue.".to_string() } else { lines.join("\n") };

                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content(content).ephemeral(true),
                    );
                    if let Err(e) = component.create_response(&ctx, response).await {
                        tracing::warn!(error = ?e, "failed to respond to button interaction");
                    }
                    return;
                }
                _ => "Unknown button",
            };

            if custom_id != "ctrl_queue_hint" {
                let response = CreateInteractionResponse::Acknowledge;
                if let Err(e) = component.create_response(&ctx, response).await {
                    tracing::warn!(error = ?e, "failed to ack button interaction");
                }
            }
            return;
        }

        let cmd = match interaction.command() {
            Some(c) => c,
            None => { tracing::warn!("interaction was not a command or component"); return; }
        };
        tracing::info!(command = %cmd.data.name, "processing slash command");

        let code_arg: Option<String> = cmd
            .data
            .options
            .iter()
            .find(|o| o.name == "code")
            .and_then(|o| {
                if let serenity::model::application::CommandDataOptionValue::String(s) = &o.value {
                    Some(s.clone())
                } else {
                    None
                }
            });

        let user_id = cmd.user.id.to_string();
        let user_id_u64 = cmd.user.id.get();
        let username = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());

        // Handle /play separately (deferred response)
        if cmd.data.name.as_str() == "play" {
            self.handle_play(&cmd, &ctx).await;
            return;
        }

        // Defer login immediately — OAuth + session startup takes >3s
        if cmd.data.name.as_str() == "login" {
            let _ = cmd.defer_ephemeral(&ctx).await;
            let reply = self.handle_login(&user_id, user_id_u64, &username, code_arg.as_deref()).await;
            let _ = cmd.edit_response(&ctx, serenity::builder::EditInteractionResponse::new().content(reply)).await;
            return;
        }

        let reply = match cmd.data.name.as_str() {
            "login" => unreachable!(),
            "logout" => self.handle_logout(&user_id, user_id_u64).await,
            "forget" => self.handle_forget(&user_id).await,
            "who" => self.handle_who().await,
            "queue" => {
                let url_arg: Option<String> = cmd
                    .data
                    .options
                    .iter()
                    .find(|o| o.name == "url")
                    .and_then(|o| {
                        if let serenity::model::application::CommandDataOptionValue::String(s) = &o.value {
                            Some(s.clone())
                        } else {
                            None
                        }
                    });
                self.handle_queue(url_arg.as_deref()).await
            }
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

impl Handler {
    async fn join_voice_for_user(&self, discord_user_id: u64) {
        let ctx = {
            let lock = self.ctx.lock().unwrap_or_else(|e| e.into_inner());
            match lock.clone() {
                Some(c) => c,
                None => { tracing::warn!("no ctx available for voice join"); return; }
            }
        };

        let user_channel = self.guild_id.to_guild_cached(&ctx)
            .and_then(|guild| {
                guild.voice_states.get(&UserId::new(discord_user_id))
                    .and_then(|vs| vs.channel_id)
            });

        let target_channel = user_channel.unwrap_or(self.channel_id);

        let manager = match songbird::get(&ctx).await {
            Some(m) => m,
            None => { tracing::error!("songbird not registered"); return; }
        };

        match manager.join(self.guild_id, target_channel).await {
            Ok(call) => {
                tracing::info!(channel = %target_channel, "joined voice channel for login");
                // Self-deafen so users know we're not listening
                let bot_id = ctx.cache.current_user().id;
                let _ = self.guild_id.edit_member(&ctx, bot_id,
                    serenity::builder::EditMember::new().deafen(true)).await;
                tracing::info!("self-deafened");
                let bridge = self.bridge.clone();
                let prebuffer_samples = self.prebuffer_samples;
                let prebuffer_wait = self.prebuffer_wait;
                let track_handle_store = self.track_handle.clone();
                let dj_join = self.dj.clone();
                tokio::spawn(play_join_sound_then_bridge(call, bridge, prebuffer_samples, prebuffer_wait, track_handle_store, dj_join));
            }
            Err(e) => tracing::warn!(error = ?e, "failed to join voice channel on login"),
        }
    }

    /// Restart the stored active user's Spotify session on boot, through the
    /// exact same path /login uses. Skips when OAuth is unconfigured, no user
    /// is marked active, or the stored record is unusable.
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

        tracing::info!(spotify = %user.spotify_username, "auto-starting stored session");
        println!("Auto-starting Spotify session for {}...", user.spotify_username);

        let (access_token, refresh_token) =
            match oauth.refresh_access_token(&user.refresh_token).await {
                Ok(t) => {
                    let mut updated = user.clone();
                    updated.access_token = t.access_token.clone();
                    if let Some(rt) = t.refresh_token.clone() {
                        updated.refresh_token = rt;
                    }
                    let _ = self.user_store.save(&updated);
                    (t.access_token, updated.refresh_token)
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "auto-start token refresh failed; using stored token");
                    (user.access_token.clone(), user.refresh_token.clone())
                }
            };

        self.spawn_session(
            discord_user_id,
            user.spotify_username.clone(),
            user.spotify_username,
            access_token,
            refresh_token,
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
    ) {
        // P1: Cancel any active YouTube/file feeder before starting Spotify session
        {
            let token = {
                let lock = self.feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                lock.clone()
            };
            if let Some(t) = token { t.cancel(); }
        }
        {
            let mut lock = self.priority_queue.lock().unwrap_or_else(|e| e.into_inner());
            lock.clear();
        }
        {
            let mut lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
            *lock = None;
        }

        let config = self.config.clone();
        let bridge = self.bridge.clone();
        let presence_tx = self.presence_tx.clone();
        let active_session = self.active_session.clone();
        let oauth_for_task = self.oauth.clone();
        let user_store_for_task = self.user_store.clone();
        let user_id_str = discord_user_id.to_string();
        let mut refresh_token = refresh_token;
        let mut access_token = access_token;

        {
            let mut lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = lock.take() {
                tracing::info!(old_user = old.discord_user_id, "aborting existing librespot session");
                old.handle.abort();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        self.join_voice_for_user(discord_user_id).await;

        {
            let ctx = {
                let lock = self.ctx.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut lock = self.spirc_cmd_tx.lock().unwrap_or_else(|e| e.into_inner());
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
            controls_message_id,
            now_playing_message_id,
        ));

        let active_session_for_task = active_session.clone();
        let spotify_name_clone = spotify_name.clone();
        let access_token_for_store = access_token.clone();
        let handle = tokio::spawn(async move {
            tracing::info!(user = discord_user_id, "librespot OAuth session starting");
            let mut spirc_rx = Some(spirc_rx);
            loop {
                match SpotifyPlayer::run_with_token(
                    &config, bridge.clone(), presence_tx.clone(), access_token.clone(),
                    Some(eot_tx.clone()),
                    spirc_rx.take(),
                ).await {
                    Ok(()) => tracing::info!(user = discord_user_id, "librespot session ended cleanly"),
                    Err(e) => tracing::warn!(user = discord_user_id, error = ?e, "librespot session ended with error"),
                }
                tracing::info!(user = discord_user_id, "attempting token refresh and reconnect in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                match oauth_for_task.refresh_access_token(&refresh_token).await {
                    Ok(new_token) => {
                        tracing::info!(user = discord_user_id, "token refreshed, reconnecting");
                        if let Some(rt) = new_token.refresh_token.clone() { refresh_token = rt; }
                        access_token = new_token.access_token;
                        if let Some(mut creds) = user_store_for_task.load(&user_id_str) {
                            creds.access_token = access_token.clone();
                            creds.refresh_token = refresh_token.clone();
                            let _ = user_store_for_task.save(&creds);
                        }
                        {
                            let mut lock = active_session_for_task.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(s) = lock.as_mut() {
                                if s.discord_user_id == discord_user_id {
                                    s.access_token = access_token.clone();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(user = discord_user_id, error = ?e, "token refresh failed");
                        break;
                    }
                }
                // spirc_rx is consumed on first call; subsequent reconnect loops pass None
                // This is handled by the Option<> in run_with_token
                // We need a fresh spirc_rx for each reconnect iteration — but since it was moved
                // into the first call, subsequent calls get None. The spirc command listener
                // from the first call dies when Spirc drops. This means pause/play commands
                // won't work after reconnect. Acceptable for v1.
                #[allow(unused_assignments)]
                {
                    // spirc_rx was moved into the first iteration; subsequent calls get None
                    // The priority queue manager's spirc_cmd_tx will error on send, which is graceful
                }
            }
            let mut lock = active_session_for_task.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = lock.as_ref() {
                if s.discord_user_id == discord_user_id { *lock = None; }
            }
        });

        let mut lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(ActiveSession {
            discord_user_id,
            spotify_name,
            discord_name,
            access_token: access_token_for_store,
            handle,
        });
        tracing::info!(user = discord_user_id, spotify = %spotify_name_clone, "librespot session spawned");
    }

    async fn handle_play(
        &self,
        cmd: &serenity::model::application::CommandInteraction,
        ctx: &Context,
    ) {
        if !self.ytdlp_available {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ YouTube playback is not available (yt-dlp not installed).")
                    .ephemeral(true)
            )).await;
            return;
        }

        let url_arg: Option<String> = cmd.data.options.iter()
            .find(|o| o.name == "url")
            .and_then(|o| if let serenity::model::application::CommandDataOptionValue::String(s) = &o.value { Some(s.clone()) } else { None });

        let attachment_arg = cmd.data.resolved.attachments.values().next().cloned();

        if url_arg.is_none() && attachment_arg.is_none() {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ Provide a YouTube URL or attach an audio file.")
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

        // Defer response
        let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true)
        )).await;

        // Build QueueItem
        let queue_item = if let Some(url) = url_arg {
            match fetch_youtube_metadata(&url).await {
                Ok(meta) => QueueItem {
                    source: MediaSource::YouTube {
                        url: meta.webpage_url.clone(),
                        video_id: meta.video_id,
                        title: meta.title,
                        channel: meta.channel,
                        thumbnail_url: meta.thumbnail_url,
                        duration_secs: meta.duration_secs,
                    },
                    queued_by: discord_name.clone(),
                    queued_by_id: discord_id,
                },
                Err(e) => {
                    let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
                        .content(format!("❌ {}", e))
                    ).await;
                    return;
                }
            }
        } else {
            let att = attachment_arg.unwrap();
            match validate_attachment(&att.filename, att.size as u64) {
                Ok(_ext) => QueueItem {
                    source: MediaSource::File {
                        filename: att.filename.clone(),
                        attachment_url: att.url.clone(),
                        content_type: att.content_type.clone(),
                    },
                    queued_by: discord_name.clone(),
                    queued_by_id: discord_id,
                },
                Err(e) => {
                    let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
                        .content(format!("❌ {}", e))
                    ).await;
                    return;
                }
            }
        };

        let title = queue_item.source.display_title().to_string();

        let is_priority_playing = {
            let lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
            lock.is_some()
        };

        let queue_len = {
            let mut lock = self.priority_queue.lock().unwrap_or_else(|e| e.into_inner());
            lock.push(queue_item.clone());
            lock.len()
        };

        // Always play immediately if nothing is actively playing
        // (even if a Spotify session exists but is idle/paused)
        let reply = if is_priority_playing {
            format!("✅ Added to queue: **{}** · Position #{}", title, queue_len)
        } else {
            // Nothing actively playing — start immediately
            self.trigger_priority_queue_drain().await;
            format!("▶ Playing: **{}**", title)
        };

        let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
            .content(reply)
        ).await;
    }

    async fn trigger_priority_queue_drain(&self) {
        let pq = self.priority_queue.clone();
        let bridge = self.bridge.clone();
        let ctx_arc = self.ctx.clone();
        let text_channel_id = self.text_channel_id;
        let active_priority_item = self.active_priority_item.clone();
        let feeder_cancel = self.feeder_cancel.clone();
        let feeder_paused = self.feeder_paused.clone();
        let controls_message_id = self.controls_message_id.clone();
        let now_playing_message_id = self.now_playing_message_id.clone();

        // Ensure bot is in voice
        let ctx = {
            let lock = self.ctx.lock().unwrap_or_else(|e| e.into_inner());
            lock.clone()
        };
        if let Some(ctx) = &ctx {
            let manager = songbird::get(ctx).await;
            if let Some(manager) = manager {
                // Check if already in a call
                let in_call = manager.get(self.guild_id).is_some();
                if !in_call {
                    // Join the queuing user's voice channel, fallback to configured
                    let user_channel = self.guild_id.to_guild_cached(ctx)
                        .and_then(|guild| {
                            // Find any human in a voice channel to follow
                            let bot_id = ctx.cache.current_user().id;
                            guild.voice_states.values()
                                .filter(|vs| vs.user_id != bot_id)
                                .filter_map(|vs| vs.channel_id)
                                .next()
                        })
                        .unwrap_or(self.channel_id);
                    match manager.join(self.guild_id, user_channel).await {
                        Ok(call) => {
                            let bridge_clone = self.bridge.clone();
                            let prebuffer_samples = self.prebuffer_samples;
                            let prebuffer_wait = self.prebuffer_wait;
                            let track_handle_store = self.track_handle.clone();
                            let dj_join2 = self.dj.clone();
                            tokio::spawn(play_join_sound_then_bridge(call, bridge_clone, prebuffer_samples, prebuffer_wait, track_handle_store, dj_join2));
                            // Wait for join sound + bridge setup
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(e) => tracing::warn!(error = ?e, "failed to join voice for standalone play"),
                    }
                }
            }
        }

        // P0: Pause Spotify before feeding YouTube/file audio to the bridge
        let spirc_cmd_tx_for_drain = {
            let lock = self.spirc_cmd_tx.lock().unwrap_or_else(|e| e.into_inner());
            lock.clone()
        };
        if let Some(ref tx) = spirc_cmd_tx_for_drain {
            let _ = tx.send(SpircCommand::Pause);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        self.bridge.clear();

        let spirc_resume_tx = spirc_cmd_tx_for_drain;
        let dj_for_drain = self.dj.clone();

        tokio::spawn(async move {
            loop {
                let item = {
                    let mut lock = pq.lock().unwrap_or_else(|e| e.into_inner());
                    lock.pop()
                };
                let item = match item {
                    Some(i) => i,
                    None => break,
                };

                {
                    let mut lock = active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                    *lock = Some(item.clone());
                }

                post_priority_now_playing(
                    &ctx_arc, text_channel_id, &item,
                    &controls_message_id, &now_playing_message_id,
                ).await;

                // DJ announcement before track
                if dj_for_drain.is_enabled() {
                    let title = item.source.display_title().to_string();
                    let subtitle = item.source.display_subtitle().to_string();
                    let queued_by = item.queued_by.clone();
                    if let Some(clip) = dj_for_drain.track_announce_clip(&title, &subtitle, &queued_by).await {
                        bridge.push_overlay(&clip);
                    }
                }
                let token = CancellationToken::new();
                {
                    let mut lock = feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                    *lock = Some(token.clone());
                }
                feeder_paused.store(false, Ordering::Relaxed);

                let result = match &item.source {
                    MediaSource::YouTube { url, .. } => {
                        feed_youtube_to_bridge(url, bridge.clone(), token, feeder_paused.clone()).await
                    }
                    MediaSource::File { attachment_url, filename, .. } => {
                        let ext = filename.rsplit('.').next().unwrap_or("mp3");
                        feed_file_to_bridge(attachment_url, ext, bridge.clone(), token, feeder_paused.clone()).await
                    }
                };

                post_priority_history(&ctx_arc, text_channel_id, &item).await;

                {
                    let mut lock = active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
                    *lock = None;
                }

                if let Err(FeederError::Cancelled) = result {
                    break;
                }
            }

            // P0: Resume Spotify after priority queue drains
            if let Some(ref tx) = spirc_resume_tx {
                let _ = tx.send(SpircCommand::Play);
            }
        });
    }

    async fn handle_skip(&self) -> String {
        let priority_playing = {
            let lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
            lock.is_some()
        };

        if priority_playing {
            let token = {
                let lock = self.feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                lock.clone()
            };
            if let Some(t) = token {
                t.cancel();
            }
            self.bridge.clear();
            // Check if more items exist; if so, start the next one
            let has_more = {
                let lock = self.priority_queue.lock().unwrap_or_else(|e| e.into_inner());
                !lock.snapshot().is_empty()
            };
            if has_more {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                self.trigger_priority_queue_drain().await;
            } else {
                // No more items — resume Spotify if session exists
                let spirc_tx = {
                    let lock = self.spirc_cmd_tx.lock().unwrap_or_else(|e| e.into_inner());
                    lock.clone()
                };
                if let Some(ref tx) = spirc_tx {
                    let _ = tx.send(SpircCommand::Play);
                }
            }
            "⏭ Skipped.".to_string()
        } else {
            let access_token = {
                let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                lock.as_ref().map(|s| s.access_token.clone())
            };
            match access_token {
                Some(token) => {
                    spotify_playback_command(&token, "POST", "next").await;
                    "⏭ Skipped.".to_string()
                }
                None => "Nothing is playing.".to_string()
            }
        }
    }

    async fn handle_stop(&self) -> String {
        let token = {
            let lock = self.feeder_cancel.lock().unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
            lock.clone()
        };
        if let Some(t) = token {
            t.cancel();
        }

        {
            let mut lock = self.priority_queue.lock().unwrap_or_else(|e| e.into_inner());
            lock.clear();
        }

        {
            let mut lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
            *lock = None;
        }

        self.bridge.clear();

        // Resume Spotify if a session exists
        let spirc_tx = {
            let lock = self.spirc_cmd_tx.lock().unwrap_or_else(|e| e.into_inner());
            lock.clone()
        };
        if let Some(ref tx) = spirc_tx {
            let _ = tx.send(SpircCommand::Play);
        }

        "⏹ Stopped. Priority queue cleared.".to_string()
    }

    async fn handle_np(&self) -> String {
        let priority_item = {
            let lock = self.active_priority_item.lock().unwrap_or_else(|e| e.into_inner());
            lock.clone()
        };
        if let Some(item) = priority_item {
            return format!("🎵 Now playing: **{}** ({})",
                item.source.display_title(),
                item.source.display_subtitle());
        }

        let spotify_name = {
            let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
            lock.as_ref().map(|s| format!("Spotify session: {}", s.spotify_name))
        };
        spotify_name.unwrap_or_else(|| "Nothing is currently playing.".to_string())
    }

    async fn handle_announce(&self) -> String {
        let current = self.announce_enabled.load(Ordering::Relaxed);
        let new_val = !current;
        self.announce_enabled.store(new_val, Ordering::Relaxed);
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
        code_arg: Option<&str>,
    ) -> String {
        // Paste-back of a redirect URL / code completes a pending PKCE auth.
        if let Some(raw) = code_arg {
            return self
                .complete_login(user_id, user_id_u64, discord_username, raw)
                .await;
        }

        // No code, but stored creds exist: quick re-login by refreshing.
        if let Some(existing) = self.user_store.load(user_id) {
            return self
                .reactivate_login(user_id, user_id_u64, discord_username, existing)
                .await;
        }

        // Fresh login: issue a PKCE challenge and the authorize URL.
        let pkce = new_pkce();
        let url = self.oauth.auth_url(&pkce);
        {
            let mut pending = self.pending_auth.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(user_id_u64, (pkce, Instant::now()));
        }
        format!(
            "Connect your Spotify account:\n\n<{url}>\n\nClick the link and authorize. \
             Your browser will then try to open a page that fails to load \
             (connection refused) — that's expected. Copy that full URL from the \
             address bar and run `/login code:<that URL>` to finish."
        )
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
                    existing.spotify_username.clone(),
                    discord_username.to_string(),
                    new_token.access_token,
                    creds.refresh_token.clone(),
                )
                .await;
                tracing::info!(user = %user_id, spotify = %existing.spotify_username, "session reactivated");
                format!("Session (re)started for **{}**!", existing.spotify_username)
            }
            Err(e) => {
                tracing::warn!(error = %e, "token refresh failed on reactivation");
                format!(
                    "Couldn't refresh your Spotify token for **{}**. Run `/forget` then `/login` to re-authorize.",
                    existing.spotify_username
                )
            }
        }
    }

    /// Complete a login by exchanging the pasted authorization code, using the
    /// PKCE verifier stashed when `/login` was first invoked.
    async fn complete_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        raw: &str,
    ) -> String {
        let params = match parse_redirect(raw) {
            Ok(p) => p,
            Err(e) => return format!("Couldn't read that redirect: {e}. Paste the full URL from your browser."),
        };

        let pending = {
            let mut lock = self.pending_auth.lock().unwrap_or_else(|e| e.into_inner());
            lock.remove(&user_id_u64)
        };
        let (pkce, started) = match pending {
            Some(p) => p,
            None => return "No pending login — run `/login` first to get an authorize link.".to_string(),
        };
        if started.elapsed() > std::time::Duration::from_secs(600) {
            return "That login link expired. Run `/login` again.".to_string();
        }
        // Validate the CSRF state when the redirect carried one.
        if let Some(returned) = params.state.as_deref() {
            if returned != pkce.state {
                tracing::warn!(user = %user_id, "OAuth state mismatch");
                return "Login state mismatch — for safety, run `/login` again.".to_string();
            }
        }

        let token = match self.oauth.exchange_code(&params.code, &pkce.verifier).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "oauth code exchange failed");
                return "Failed to exchange the code with Spotify. Run `/login` to start over.".to_string();
            }
        };
        let Some(refresh_token) = token.refresh_token.clone() else {
            return "Spotify didn't return a refresh token. Run `/login` again.".to_string();
        };
        let display_name = match self.oauth.get_user_profile(&token.access_token).await {
            Ok(name) => name,
            Err(e) => {
                tracing::warn!(error = %e, "profile fetch failed");
                "Unknown".to_string()
            }
        };
        let creds = UserCredentials {
            discord_user_id: user_id.to_string(),
            spotify_username: display_name.clone(),
            access_token: token.access_token.clone(),
            refresh_token,
            paired_at: unix_timestamp_str(),
            active: true,
        };
        if let Err(e) = self.user_store.save(&creds) {
            tracing::error!(error = %e, "failed to save credentials");
            return "Failed to save credentials. Please try again.".to_string();
        }
        tracing::info!(user = %user_id, spotify = %display_name, "oauth login successful");
        self.spawn_session(
            user_id_u64,
            display_name.clone(),
            discord_username.to_string(),
            token.access_token,
            creds.refresh_token.clone(),
        )
        .await;
        format!("Logged in as **{display_name}**! Spotify session started.")
    }

    async fn handle_logout(&self, user_id: &str, user_id_u64: u64) -> String {
        {
            let mut lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = lock.as_ref() {
                if session.discord_user_id == user_id_u64 {
                    session.handle.abort();
                    *lock = None;
                    tracing::info!(user = %user_id, "active librespot session aborted");
                }
            }
        }
        let _ = self.presence_tx.send(PresenceUpdate::Idle);

        {
            let ctx = {
                let lock = self.ctx.lock().unwrap_or_else(|e| e.into_inner());
                lock.clone()
            };
            if let Some(ctx) = ctx {
                delete_and_repost_controls(&ctx, self.text_channel_id, &self.controls_message_id, None).await;
            }
        }

        match self.user_store.deactivate(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "session deactivated"); "Session deactivated. Your credentials are kept — run `/login` to reactivate without re-authorizing.".to_string() }
            Ok(false) => "You don't have an active session.".to_string(),
            Err(e) => { tracing::error!("failed to deactivate session: {}", e); "Failed to deactivate session.".to_string() }
        }
    }

    async fn handle_forget(&self, user_id: &str) -> String {
        match self.user_store.remove(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "credentials forgotten"); "Credentials permanently deleted. Run `/login` to connect again.".to_string() }
            Ok(false) => "No stored credentials to delete.".to_string(),
            Err(e) => { tracing::error!("failed to delete credentials: {}", e); "Failed to delete credentials.".to_string() }
        }
    }

    async fn handle_who(&self) -> String {
        tracing::info!("handle_who: attempting lock");
        let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("handle_who: lock acquired");
        match lock.as_ref() {
            Some(session) => format!("Active session: **{}** (Discord user {})", session.spotify_name, session.discord_user_id),
            None => "No active Spotify session. Run `/login` to connect.".to_string(),
        }
    }

    async fn handle_queue(&self, url_arg: Option<&str>) -> String {
        let url_str = match url_arg {
            Some(u) => u,
            None => return "Please provide a Spotify track URL or URI.".to_string(),
        };

        let track_id = match parse_track_id_from_url(url_str) {
            Some(id) => id,
            None => return "Couldn't parse a Spotify track from that input. Use a URL like `https://open.spotify.com/track/...` or `spotify:track:...`".to_string(),
        };

        let access_token = {
            let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
            lock.as_ref().map(|s| s.access_token.clone())
        };

        let token = match access_token {
            Some(t) => t,
            None => return "No active Spotify session. Use /login first.".to_string(),
        };

        let uri = format!("spotify:track:{}", track_id);
        let client = reqwest::Client::new();
        let url = format!(
            "https://api.spotify.com/v1/me/player/queue?uri={}",
            uri.replace(":", "%3A")
        );

        match client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Length", "0")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 204 || status == 200 {
                    "✅ Added to queue!".to_string()
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!(status, body = %body, "queue API error");
                    format!("Failed to add to queue (HTTP {})", status)
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "queue API request failed");
                "Failed to reach Spotify API.".to_string()
            }
        }
    }
}

fn unix_timestamp_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("unix:{}", secs)
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

        let prebuffer_samples =
            (config.prebuffer_seconds * SAMPLE_RATE as f32) as usize * CHANNELS as usize;
        let prebuffer_wait =
            std::time::Duration::from_secs_f32((config.prebuffer_seconds + 0.5).clamp(0.0, 5.0));

        let active_session = Arc::new(Mutex::new(None::<ActiveSession>));
        let track_handle: Arc<Mutex<Option<TrackHandle>>> = Arc::new(Mutex::new(None));

        let dj = Arc::new(DJAnnouncer::new());
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
            track_handle,
            ctx: Arc::new(Mutex::new(None)),
            controls_message_id: Arc::new(Mutex::new(None)),
            now_playing_message_id: Arc::new(Mutex::new(None)),
            // YouTube/file fields
            ytdlp_available,
            priority_queue: Arc::new(Mutex::new(PriorityQueue::new())),
            spirc_cmd_tx: Arc::new(Mutex::new(None)),
            active_priority_item: Arc::new(Mutex::new(None)),
            feeder_cancel: Arc::new(Mutex::new(None)),
            feeder_paused: Arc::new(AtomicBool::new(false)),
            dj,
            announce_enabled: Arc::new(AtomicBool::new(false)),
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
