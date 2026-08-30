use super::presence::run_presence_loop;
use super::ui::{self, CardView, HistoryView, UiMsg};
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio::generate_join_sound;
use crate::audio::dj::DJAnnouncer;
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::{DeviceAuthorization, SpotifyOAuth};
use crate::player::actor::{self as player_actor, JoinVoiceFn, PlayerDeps, PlayerHandle, UiEvent};
use crate::player::state::{EnqueuePos, Input as PlayerInput, NowPlaying, TransportEvent};
use crate::presence::PresenceUpdate;
use crate::queue::{QueueItem, MediaSource};
use crate::spotify::SpircCommand;
use crate::spotify::SessionSupervisor;
use crate::youtube::metadata::{fetch_youtube_metadata, validate_attachment};
use crate::users::{UserCredentials, UserStore};
use serenity::all::{
    ChannelId, CreateCommand, CreateInteractionResponse,
    UserId,
    CreateInteractionResponseMessage, GatewayIntents, GuildId, Interaction, Ready,
};
use serenity::async_trait;
use serenity::builder::{CreateCommandOption, CreateMessage, EditInteractionResponse};
use serenity::client::{Client, Context, EventHandler};
use serenity::model::application::CommandOptionType;
use serenity::model::voice::VoiceState;
use songbird::events::{Event, TrackEvent};
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::SerenityInit;
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::pin::Pin;
// parking_lot: no lock poisoning, so no unwrap_or_else(into_inner)
// incantation at every acquisition (audio_bridge already uses it).
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Notify;

type ReadySignal = Result<(), String>;

/// How long to poll Spotify for a device-code pairing before giving up.
const DEVICE_LOGIN_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Track metadata as delivered by the librespot session itself (no Web API
/// calls — those all 429 under the desktop client ID).
#[derive(Clone)]
pub(crate) struct SpotifyTrackInfo {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album_art_url: Option<String>,
}

/// Display-only cache of who the live Spotify session belongs to, for `/who`
/// and the takeover gate. The `SessionSupervisor` owns the actual session
/// lifecycle (the librespot task, its refresher, the generation) — this is
/// just the name and id the callers of `switch_active_session` /
/// `supervisor.stop` populate and clear alongside it.
pub struct ActiveSession {
    pub discord_user_id: u64,
    pub discord_name: String,
}

struct Handler {
    guild_id: GuildId,
    text_channel_id: ChannelId,
    config: Arc<Config>,
    ready_tx: mpsc::Sender<ReadySignal>,
    presence_rx: Mutex<Option<mpsc::UnboundedReceiver<PresenceUpdate>>>,
    user_store: Arc<UserStore>,
    oauth: Arc<SpotifyOAuth>,
    /// Pending device-code pairings keyed by Discord user; notifying cancels
    /// that user's poll. In-memory only: a restart drops pending pairings and
    /// the user re-runs `/login`.
    pending_auth: Arc<Mutex<HashMap<u64, Arc<tokio::sync::Notify>>>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    /// Owns the Spotify session lifecycle (librespot task, refresher, token
    /// state, generation) independently of playback — see
    /// `spotify::session`. `switch_active_session`/`handle_logout`/
    /// `teardown_playback_session` are its only callers.
    supervisor: SessionSupervisor,
    ctx: Arc<Mutex<Option<Context>>>,
    /// The UI task's mailbox. `None` until `ready()`'s first pass spawns
    /// the task (see `ui::spawn`); every send site resolves it fresh via
    /// `.lock().clone()`, mirroring `ctx` above.
    ui_tx: Arc<Mutex<Option<mpsc::UnboundedSender<UiMsg>>>>,
    /// The player actor's mailbox: every playback-affecting command (/play,
    /// /queue, /skip, /stop, ⏯, ⏮, /np) and event (transport, media end,
    /// voice) funnels through it, so decide-then-act is serialized. The
    /// actor owns all playback state — anything here that needs it asks via
    /// `query()`.
    player: PlayerHandle,
    /// The same ensure-voice dispatch the actor's `JoinVoice` effect runs
    /// (a no-op when a call already exists), for the session-switch path.
    join_voice: JoinVoiceFn,
    ytdlp_available: bool,
    announce_enabled: Arc<AtomicBool>,
    /// Last /play per user, for the metadata-probe cooldown.
    play_cooldowns: Arc<Mutex<HashMap<u64, Instant>>>,
    auto_start_attempted: AtomicBool,
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

// --- Player-actor wiring: transport shim, notices, voice join ---

/// Transport shim: one long-lived event stream out of every generation of
/// librespot session (the sender lives in the `SessionSupervisor`, spawned
/// once at startup — lifecycle (A), not per-login), forwarded to the player
/// actor as `Input::Transport`. Because the channel outlives any one
/// session, the generation to stamp on each event is read fresh off
/// `link_up` (written by each session's own task without taking the
/// supervisor's lock — see `spotify::session`) rather than fixed at spawn
/// time; `unwrap_or(0)` only matters for a stray event racing a
/// link-down-to-link-up transition; the actor's own `link_gen` check is
/// still what makes a mistagged straggler harmless.
async fn transport_shim(
    mut rx: mpsc::UnboundedReceiver<TransportEvent>,
    player: PlayerHandle,
    link_up: watch::Receiver<Option<u64>>,
) {
    while let Some(ev) = rx.recv().await {
        let gen = link_up.borrow().unwrap_or(0);
        player.send(PlayerInput::Transport { gen, ev });
    }
}

/// Text-channel notices from the player actor and its media runners
/// (feeder failures, takeover prompts). A task because the actual send
/// needs the serenity `Context` and an await, which the actor must never
/// hold; messages arriving before the gateway is ready are dropped with a
/// log.
fn spawn_notice_task(
    ctx_store: Arc<Mutex<Option<Context>>>,
    text_channel_id: ChannelId,
) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let ctx = { ctx_store.lock().clone() };
            match ctx {
                Some(ctx) => {
                    let msg = CreateMessage::new().content(text);
                    let _ = text_channel_id.send_message(&ctx, msg).await;
                }
                None => tracing::warn!(%text, "notice dropped (gateway not ready)"),
            }
        }
    });
    tx
}

/// Join the given user's voice channel (falling back to the configured
/// channel), self-deafen, unsuppress on stage channels, and start the
/// join-sound + bridge hookup. Returns whether the join succeeded. Free
/// function so the Handler and the player actor's `JoinVoice` effect share
/// one implementation.
#[allow(clippy::too_many_arguments)]
async fn join_voice_inner(
    ctx_store: Arc<Mutex<Option<Context>>>,
    guild_id: GuildId,
    fallback_channel: ChannelId,
    bridge: Arc<AudioBridge>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
    dj: Arc<DJAnnouncer>,
    discord_user_id: Option<u64>,
) -> bool {
    let ctx = {
        let lock = ctx_store.lock();
        match lock.clone() {
            Some(c) => c,
            None => { tracing::warn!("no ctx available for voice join"); return false; }
        }
    };

    let user_channel = discord_user_id.and_then(|id| {
        guild_id.to_guild_cached(&ctx)
            .and_then(|guild| {
                guild.voice_states.get(&UserId::new(id))
                    .and_then(|vs| vs.channel_id)
            })
    });

    let target_channel = user_channel.unwrap_or(fallback_channel);

    let manager = match songbird::get(&ctx).await {
        Some(m) => m,
        None => { tracing::error!("songbird not registered"); return false; }
    };

    match manager.join(guild_id, target_channel).await {
        Ok(call) => {
            tracing::info!(channel = %target_channel, "joined voice channel");
            // Self-deafen so users know we're not listening
            let bot_id = ctx.cache.current_user().id;
            let _ = guild_id.edit_member(&ctx, bot_id,
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
            tokio::spawn(play_join_sound_then_bridge(call, bridge, prebuffer_samples, prebuffer_wait, track_handle_store, dj));
            true
        }
        Err(e) => {
            tracing::warn!(error = ?e, "failed to join voice channel");
            false
        }
    }
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
        // resume/reconnect; spawning a second UI task then would orphan the
        // live card and race the first task over the same channel messages.
        let first_ready = !self.auto_start_attempted.swap(true, Ordering::SeqCst);
        if first_ready {
            // The task's own startup (stale-message sweep, idle card post)
            // runs before it drains its mailbox, so sends queued below by
            // auto_start_stored_session can never race ahead of it — see
            // `ui::run`.
            let tx = ui::spawn(ctx.clone(), self.text_channel_id);
            *self.ui_tx.lock() = Some(tx);
        }

        let rx_taken = {
            let mut presence_rx = self.presence_rx.lock();
            presence_rx.take()
        };
        if let Some(rx) = rx_taken {
            // The status task: renders the actor-fed `PresenceUpdate`s as
            // the bot's Discord activity line.
            tokio::spawn(run_presence_loop(ctx.clone(), rx));
        }

        // Auto-start: replay the stored active user's session through the same
        // machinery /login uses (voice join, controls, refresh loop). Runs
        // only on the first ready (see `first_ready` above).
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
                    let playback =
                        !matches!(self.player.query().await.now, NowPlaying::Nothing);
                    session || playback
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
                "ctrl_prev" => self.player.previous().await,
                // Same semantics as /skip: the actor cancels the current
                // media item or advances whatever the queue head says.
                "ctrl_next" => self.player.skip().await,
                // ⏯: the actor pauses/resumes the active media item, pauses
                // a playing baseline, or starts/resumes whatever is next.
                "ctrl_pause_toggle" => self.player.toggle_pause().await,
                "ctrl_queue_hint" => {
                    let content = self.format_queue_listing().await;

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
            "skip" => self.player.skip().await,
            "stop" => self.player.stop().await,
            "np" => render_now_playing(&self.player.query().await.now),
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

/// Renders the actor's view of what's audible, for `/np` and the queue
/// listing's status line. Follows the reply house style documented on
/// `player::state`'s `reply`: `▶`/`⏸` for the transport state, track and
/// user names in bold, one phrasing per state.
fn render_now_playing(now: &NowPlaying) -> String {
    match now {
        NowPlaying::Nothing => "Nothing is playing right now.".to_string(),
        NowPlaying::Media { title, subtitle, queued_by, paused } => {
            let glyph = if *paused { "⏸" } else { "▶" };
            format!("{glyph} **{title}** — {subtitle} · queued by **{queued_by}**")
        }
        NowPlaying::Spotify { title, artist, paused } => {
            let glyph = if *paused { "⏸" } else { "▶" };
            format!("{glyph} **{title}** — {artist}")
        }
        NowPlaying::SpotifyStarting => "▶ Starting Spotify playback…".to_string(),
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

    /// The queue listing shown by the `ctrl_queue_hint` button and by
    /// `/queue` with no arguments, rendered from the actor's
    /// `PlayerSnapshot`: how to add tracks, what's audible right now, and
    /// the first few queued items (with a `+N more` line for the rest).
    async fn format_queue_listing(&self) -> String {
        let snap = self.player.query().await;
        let mut lines = vec![];
        if snap.link_up {
            lines.push("Use `/queue <spotify_url>` to add Spotify tracks.".to_string());
        }
        if self.ytdlp_available {
            lines.push("Use `/play <youtube_url>` to add YouTube tracks.".to_string());
        }
        lines.push(render_now_playing(&snap.now));
        if snap.queue_len > 0 {
            lines.push(format!("\nQueue ({} item(s)):", snap.queue_len));
            for (i, entry) in snap.preview.iter().enumerate() {
                let duration = entry
                    .duration
                    .as_ref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                let armed = if entry.armed { " ⏭ next on Spotify" } else { "" };
                lines.push(format!(
                    "  {}. **{}** — {}{} · queued by {}{}",
                    i + 1,
                    entry.title,
                    entry.subtitle,
                    duration,
                    entry.queued_by,
                    armed
                ));
            }
            if snap.more > 0 {
                lines.push(format!("  +{} more", snap.more));
            }
        }
        lines.join("\n")
    }

    /// Full playback teardown: silence the player (media cancelled, queue
    /// cleared), abort any Spotify session (deactivating its owner), reset
    /// the controls card, and optionally leave voice. Runs when the voice
    /// channel empties and when the bot is force-disconnected.
    async fn teardown_playback_session(&self, ctx: &Context, leave_voice: bool) {
        // VoiceLost first (mailbox order beats the runner's own cancel
        // report): the actor drops any active media turn and stale-ifies
        // the runner's coming `MediaEnded`. The awaited Stop then clears
        // the queue before the supervisor's `LinkDown` lands, so nothing
        // gets promoted into the emptying call, and the actor's own
        // presence/status transitions cover the Idle update.
        self.player.send(PlayerInput::VoiceLost);
        let _ = self.player.stop().await;

        let owner = {
            let mut lock = self.active_session.lock();
            lock.take().map(|session| session.discord_user_id)
        };
        if let Some(owner) = owner {
            self.supervisor.stop(owner).await;
            tracing::info!(user = owner, "aborted session (teardown)");
        }

        let tx = { self.ui_tx.lock().clone() };
        if let Some(tx) = tx {
            let _ = tx.send(UiMsg::Idle { account: None });
        }

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

    /// Whether a user may queue via /play: if the bot is already in a channel,
    /// they must share it (the control rule); if the bot is in no channel yet,
    /// they only need to be in one so the bot can follow them in.
    fn user_can_play(&self, ctx: &Context, user_id: UserId) -> bool {
        let (bot_ch, user_ch) = self.voice_channels(ctx, user_id);
        voice_gate(bot_ch, user_ch, true)
    }

    /// Ensure the bot is in a voice call, following `discord_user_id` in
    /// when it has to join fresh — a no-op when a call already exists, so a
    /// session switch never replays the join sound or re-hooks the bridge
    /// reader over a call that's already up (which would cut whatever media
    /// item is currently feeding it).
    async fn ensure_voice_for_user(&self, discord_user_id: Option<u64>) -> bool {
        let ctx = { self.ctx.lock().clone() };
        if let Some(ctx) = &ctx {
            if let Some(manager) = songbird::get(ctx).await {
                if manager.get(self.guild_id).is_some() {
                    return true;
                }
            }
        }
        (self.join_voice)(discord_user_id).await
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

        self.switch_active_session(
            discord_user_id,
            user.discord_name,
            access_token,
            refresh_token,
            expires_in,
        )
        .await;
    }

    /// Point the live Spotify session at `discord_user_id` and update
    /// everything downstream of an account change: the DB's exclusive-active
    /// flag, the `/who`/takeover-gate display cache, the voice call (a no-op
    /// when one already exists), and the card's account name. Never touches
    /// the player — a media item already playing keeps playing straight
    /// through a login, and the actor drops the replaced session's armed
    /// track itself when the new session's `LinkUp` reaches it.
    async fn switch_active_session(
        &self,
        discord_user_id: u64,
        discord_name: String,
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    ) {
        self.supervisor
            .switch(discord_user_id, discord_name.clone(), access_token, refresh_token, expires_in)
            .await;

        // Exactly one user stays active:true, so auto-start can't resurrect a
        // displaced user after a restart.
        if let Err(e) = self.user_store.set_active_exclusive(&discord_user_id.to_string()) {
            tracing::warn!(error = %e, "failed to set exclusive active user");
        }

        {
            let mut lock = self.active_session.lock();
            *lock = Some(ActiveSession { discord_user_id, discord_name: discord_name.clone() });
        }

        let _ = self.ensure_voice_for_user(Some(discord_user_id)).await;

        let tx = { self.ui_tx.lock().clone() };
        if let Some(tx) = tx {
            let _ = tx.send(UiMsg::AccountChanged(Some(discord_name)));
        }
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
    /// used for Spotify links, whose metadata resolves through the live
    /// session (`PlayerHandle::lookup_spotify`) instead.
    async fn build_media_queue_item(
        url: Option<String>,
        attachment: Option<serenity::model::channel::Attachment>,
        discord_name: &str,
        discord_id: u64,
    ) -> Result<QueueItem, String> {
        if let Some(url) = url {
            let meta = fetch_youtube_metadata(&url).await.map_err(|e| e.to_string())?;
            Ok(QueueItem {
            item_id: 0,
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
            item_id: 0,
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
        // immediately instead of deferring. Metadata resolves here in the
        // handler task (never inside the actor); the actor then owns the
        // enqueue-and-maybe-start decision and the reply.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let reply = if !self.player.has_session() {
                    "No Spotify session — run `/login` to connect.".to_string()
                } else {
                    match self.player.lookup_spotify(&spotify_uri).await {
                        None => "⚠️ Couldn't resolve that Spotify track — check the link and try again.".to_string(),
                        Some((title, artist, album_art_url)) => {
                            let item = QueueItem {
                                item_id: 0,
                                source: MediaSource::Spotify { uri: spotify_uri, title, artist, album_art_url },
                                queued_by: discord_name.clone(),
                                queued_by_id: discord_id,
                            };
                            let pos = if next {
                                // An armed head is already on Spotify's own
                                // device queue and can't be un-queued — a
                                // "next" item lands right behind it instead
                                // of jumping it, so the listing matches the
                                // air order.
                                let head_armed = self
                                    .player
                                    .query()
                                    .await
                                    .preview
                                    .first()
                                    .is_some_and(|entry| entry.armed);
                                if head_armed { EnqueuePos::At(1) } else { EnqueuePos::Head }
                            } else {
                                EnqueuePos::Tail
                            };
                            self.player.enqueue(item, pos, true).await
                        }
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

        // The actor owns the enqueue-and-maybe-start decision from here: it
        // pushes into its owned queue, starts the head when nothing holds
        // the turn, and formats the reply.
        let reply = self
            .player
            .enqueue(
                queue_item,
                if next { EnqueuePos::Head } else { EnqueuePos::Tail },
                true,
            )
            .await;

        let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
            .content(reply)
        ).await;
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
                self.switch_active_session(
                    user_id_u64,
                    discord_username.to_string(),
                    new_token.access_token,
                    creds.refresh_token.clone(),
                    expires_in,
                )
                .await;
                tracing::info!(user = %user_id, name = %discord_username, "session reactivated");
                format!(
                    "Session (re)started for **{}**! Pick **{}** in Spotify's device list to play.",
                    discord_username, self.config.device_name
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
        tracing::info!(user = %user_id, name = %display_name, "device login successful");
        self.switch_active_session(
            user_id_u64,
            display_name.clone(),
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

        // Only the owner of the live session may tear it down. A bystander's
        // /logout must not pause the DJ's audio or wipe the controls. The
        // supervisor re-checks ownership itself (`stop` is a no-op for a
        // non-owner); this flag is only for which reply text to show below.
        let owned_live_session = self.active_owner() == Some(user_id_u64);

        if owned_live_session {
            self.supervisor.stop(user_id_u64).await;
            {
                let mut lock = self.active_session.lock();
                *lock = None;
            }
            tracing::info!(user = %user_id, "active librespot session aborted");
            // Does NOT touch playback directly — the supervisor's `stop`
            // emits `LinkDown`, and the actor (the sole owner of the queue,
            // the armed track and the status line) decides what a dead link
            // means; a queued media item keeps playing straight through a
            // logout.
            let tx = { self.ui_tx.lock().clone() };
            if let Some(tx) = tx {
                let _ = tx.send(UiMsg::Idle { account: None });
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
            // One name: the Web API profile lookup this used to pair with
            // Discord's own name is gone (429s under the desktop client id),
            // so there is only ever the one name to show.
            Some(session) => format!("Active session: **{}**", session.discord_name),
            None => "No active Spotify session. Run `/login` to connect.".to_string(),
        }
    }

    /// `/queue`: adds to the queue's tail without starting playback —
    /// Spotify, YouTube/SoundCloud and attachments all land in the actor's
    /// one unified queue (PORT.md decision #15), never jump the line, never
    /// start playback. No arguments shows the current queue listing.
    async fn handle_queue(
        &self,
        cmd: &serenity::model::application::CommandInteraction,
        ctx: &Context,
    ) {
        let (url_arg, attachment_arg, _next) = Self::parse_play_queue_options(cmd);

        if url_arg.is_none() && attachment_arg.is_none() {
            let content = self.format_queue_listing().await;
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

        let discord_name = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let discord_id = cmd.user.id.get();

        // Spotify track link: no yt-dlp probe, no defer needed. Metadata
        // resolves here in the handler task (never inside the actor); the
        // actor owns the tail push and the reply, with `start_if_idle` off.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let reply = if !self.player.has_session() {
                    "No Spotify session — run `/login` to connect.".to_string()
                } else {
                    match self.player.lookup_spotify(&spotify_uri).await {
                        None => "⚠️ Couldn't resolve that Spotify track — check the link and try again.".to_string(),
                        Some((title, artist, album_art_url)) => {
                            let item = QueueItem {
                                item_id: 0,
                                source: MediaSource::Spotify { uri: spotify_uri, title, artist, album_art_url },
                                queued_by: discord_name.clone(),
                                queued_by_id: discord_id,
                            };
                            self.player.enqueue(item, EnqueuePos::Tail, false).await
                        }
                    }
                };
                let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(reply).ephemeral(true)
                )).await;
                return;
            }
        }

        // YouTube/SoundCloud URL, or a file attachment: goes on the queue's
        // tail via the same metadata probe /play uses.
        if url_arg.is_some() && !self.ytdlp_available {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ YouTube playback is not available (yt-dlp not installed).")
                    .ephemeral(true)
            )).await;
            return;
        }

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

        // `/queue` never starts playback: the actor pushes to the tail and
        // formats the reply, with `start_if_idle` off.
        let reply = self.player.enqueue(queue_item, EnqueuePos::Tail, false).await;

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

        // Cross-task seams shared between the actor's deps, the session
        // supervisor and the Handler, hoisted so each holds the same `Arc`s.
        let guild_id = GuildId::new(config.discord_guild_id);
        let channel_id = ChannelId::new(config.discord_channel_id);
        let text_channel_id = ChannelId::new(config.discord_text_channel_id);
        let ctx_store: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
        let ui_tx_store: Arc<Mutex<Option<mpsc::UnboundedSender<UiMsg>>>> =
            Arc::new(Mutex::new(None));
        let spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>> =
            Arc::new(Mutex::new(None));
        // Restore the persisted /announce toggle so restarts (including
        // the VPS updater's) don't silently disable announcements.
        let announce_enabled = Arc::new(AtomicBool::new(
            announce_persisted.as_deref() == Some("1"),
        ));

        // Synchronous UI dispatch for the actor: resolve the UI task's
        // mailbox fresh per send (it exists only after ready()) and map the
        // actor's events onto the UI task's own message type, which is
        // private to this module. The Spotify card's DJ footer comes from
        // the `/who` display cache, the one account-name surface there is.
        let ui_send: player_actor::UiSendFn = {
            let ui_tx = ui_tx_store.clone();
            let active_session = active_session.clone();
            Arc::new(move |event: UiEvent| {
                let tx = { ui_tx.lock().clone() };
                if let Some(tx) = tx {
                    let _ = tx.send(match event {
                        UiEvent::NowPlayingMedia { item } => {
                            UiMsg::NowPlaying(CardView::Queued { item })
                        }
                        UiEvent::NowPlayingSpotify { uri, meta } => {
                            let dj_name = {
                                let lock = active_session.lock();
                                lock.as_ref().map(|s| s.discord_name.clone()).unwrap_or_default()
                            };
                            let (title, artist, album_art_url) = match meta {
                                Some(m) => (m.title, m.artist, m.album_art_url),
                                None => (
                                    "Unknown track".to_string(),
                                    "Unknown artist".to_string(),
                                    None,
                                ),
                            };
                            UiMsg::NowPlaying(CardView::Spotify {
                                title,
                                artist,
                                track_id: uri.to_id(),
                                album_art_url,
                                dj_name,
                            })
                        }
                        UiEvent::HistoryMedia { item } => {
                            UiMsg::History(HistoryView::Queued { item })
                        }
                        UiEvent::IdleCard => UiMsg::Idle { account: None },
                        UiEvent::Buttons { paused } => UiMsg::Buttons { paused },
                    });
                }
            })
        };

        // Voice-join dispatch for the actor's `JoinVoice` effect: a no-op
        // when the bot is already in a call (the reader is hooked and the
        // join sound played), a fresh follow-the-requester join otherwise.
        let join_voice: player_actor::JoinVoiceFn = {
            let ctx_store = ctx_store.clone();
            let bridge = bridge.clone();
            let track_handle = track_handle.clone();
            let dj = dj.clone();
            Arc::new(
                move |discord_user_id: Option<u64>| -> Pin<Box<dyn Future<Output = bool> + Send>> {
                    let ctx_store = ctx_store.clone();
                    let bridge = bridge.clone();
                    let track_handle = track_handle.clone();
                    let dj = dj.clone();
                    Box::pin(async move {
                        let ctx = { ctx_store.lock().clone() };
                        if let Some(ctx) = &ctx {
                            if let Some(manager) = songbird::get(ctx).await {
                                if manager.get(guild_id).is_some() {
                                    return true;
                                }
                            }
                        }
                        join_voice_inner(
                            ctx_store,
                            guild_id,
                            channel_id,
                            bridge,
                            prebuffer_samples,
                            prebuffer_wait,
                            track_handle,
                            dj,
                            discord_user_id,
                        )
                        .await
                    })
                },
            )
        };

        let notice_tx = spawn_notice_task(ctx_store.clone(), text_channel_id);

        let player = player_actor::spawn(PlayerDeps {
            bridge: bridge.clone(),
            config: config.clone(),
            ui_send,
            notice_tx,
            presence_tx,
            join_voice: join_voice.clone(),
            spirc_cmd_tx: spirc_cmd_tx.clone(),
            track_handle,
            dj,
            announce_enabled: announce_enabled.clone(),
        });

        // One long-lived transport-event stream, shared by every generation
        // of Spotify session (unlike the old per-login channel) — the
        // supervisor holds the sender, this task the receiver, forwarding
        // events to the player actor for the life of the process. The
        // `spirc_cmd_tx` cell it also receives is the same one the actor's
        // deps hold: the supervisor writes the live sender there, and the
        // actor (plus `PlayerHandle`'s lookup helper) is the only reader.
        let (transport_tx, transport_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let supervisor = SessionSupervisor::new(
            config.clone(),
            bridge,
            oauth.clone(),
            user_store.clone(),
            transport_tx,
            player.clone(),
            spirc_cmd_tx,
        );
        let link_up_watch = supervisor.link_up_watch();
        tokio::spawn(transport_shim(transport_rx, player.clone(), link_up_watch));

        let handler = Handler {
            guild_id,
            text_channel_id,
            config: config.clone(),
            ready_tx,
            presence_rx: Mutex::new(Some(presence_rx)),
            user_store,
            oauth,
            pending_auth: Arc::new(Mutex::new(HashMap::new())),
            active_session,
            supervisor,
            ctx: ctx_store,
            ui_tx: ui_tx_store,
            player,
            join_voice,
            ytdlp_available,
            announce_enabled,
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
        is_valid_track_id, parse_track_id_from_url, render_now_playing, voice_gate, NowPlaying,
    };
    use serenity::all::ChannelId;

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

    // --- render_now_playing: the /np and queue-listing status line ---

    #[test]
    fn renders_nothing_with_the_house_phrasing() {
        assert_eq!(render_now_playing(&NowPlaying::Nothing), "Nothing is playing right now.");
    }

    #[test]
    fn renders_a_media_item_with_requester_and_pause_glyph() {
        let now = NowPlaying::Media {
            title: "Song".into(),
            subtitle: "Channel".into(),
            queued_by: "DJ".into(),
            paused: false,
        };
        assert_eq!(render_now_playing(&now), "▶ **Song** — Channel · queued by **DJ**");
        let paused = NowPlaying::Media {
            title: "Song".into(),
            subtitle: "Channel".into(),
            queued_by: "DJ".into(),
            paused: true,
        };
        assert!(render_now_playing(&paused).starts_with('⏸'));
    }

    #[test]
    fn renders_a_spotify_track_with_pause_glyph() {
        let now = NowPlaying::Spotify { title: "Song".into(), artist: "Artist".into(), paused: false };
        assert_eq!(render_now_playing(&now), "▶ **Song** — Artist");
        let paused = NowPlaying::Spotify { title: "Song".into(), artist: "Artist".into(), paused: true };
        assert!(render_now_playing(&paused).starts_with('⏸'));
    }

    #[test]
    fn renders_a_pending_spotify_start() {
        assert!(render_now_playing(&NowPlaying::SpotifyStarting).starts_with('▶'));
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
