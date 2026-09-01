use super::account::ActiveSession;
use super::commands;
use super::presence::run_presence_loop;
use super::ui::{self, CardView, HistoryView, UiMsg};
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio::generate_join_sound;
use crate::audio::dj::DJAnnouncer;
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::SpotifyOAuth;
use crate::player::actor::{self as player_actor, JoinVoiceFn, PlayerDeps, PlayerHandle, UiEvent};
use crate::player::state::{Input as PlayerInput, NowPlaying, TransportEvent};
use crate::presence::PresenceUpdate;
use crate::spotify::SpircCommand;
use crate::spotify::SessionSupervisor;
use crate::users::UserStore;
use serenity::all::{ChannelId, GatewayIntents, GuildId, Interaction, Ready, UserId};
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::client::{Client, Context, EventHandler};
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

type ReadySignal = Result<(), String>;

/// Track metadata as delivered by the librespot session itself (no Web API
/// calls — those all 429 under the desktop client ID).
#[derive(Clone)]
pub(crate) struct SpotifyTrackInfo {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album_art_url: Option<String>,
}

pub(super) struct Handler {
    pub(super) guild_id: GuildId,
    pub(super) text_channel_id: ChannelId,
    pub(super) config: Arc<Config>,
    ready_tx: mpsc::Sender<ReadySignal>,
    presence_rx: Mutex<Option<mpsc::UnboundedReceiver<PresenceUpdate>>>,
    pub(super) user_store: Arc<UserStore>,
    pub(super) oauth: Arc<SpotifyOAuth>,
    /// Pending device-code pairings keyed by Discord user; notifying cancels
    /// that user's poll. In-memory only: a restart drops pending pairings and
    /// the user re-runs `/login`.
    pub(super) pending_auth: Arc<Mutex<HashMap<u64, Arc<tokio::sync::Notify>>>>,
    pub(super) active_session: Arc<Mutex<Option<ActiveSession>>>,
    /// Owns the Spotify session lifecycle (librespot task, refresher, token
    /// state, generation) independently of playback — see
    /// `spotify::session`. `switch_active_session`/`auto_start_stored_session`/
    /// `handle_logout`/`teardown_playback_session` are its only callers.
    pub(super) supervisor: SessionSupervisor,
    pub(super) ctx: Arc<Mutex<Option<Context>>>,
    /// The UI task's mailbox. `None` until `ready()`'s first pass spawns
    /// the task (see `ui::spawn`); every send site resolves it fresh via
    /// `.lock().clone()`, mirroring `ctx` above.
    pub(super) ui_tx: Arc<Mutex<Option<mpsc::UnboundedSender<UiMsg>>>>,
    /// The player actor's mailbox: every playback-affecting command (/play,
    /// /queue, /skip, /stop, ⏯, ⏮, /np) and event (transport, media end,
    /// voice) funnels through it, so decide-then-act is serialized. The
    /// actor owns all playback state — anything here that needs it asks via
    /// `query()`.
    pub(super) player: PlayerHandle,
    /// The same ensure-voice dispatch the actor's `JoinVoice` effect runs
    /// (a no-op when a call already exists), for the session-switch path.
    pub(super) join_voice: JoinVoiceFn,
    pub(super) ytdlp_available: bool,
    pub(super) announce_enabled: Arc<AtomicBool>,
    /// Last /play per user, for the metadata-probe cooldown.
    pub(super) play_cooldowns: Arc<Mutex<HashMap<u64, Instant>>>,
    auto_start_attempted: AtomicBool,
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

/// Whether the bot is actually connected to a voice channel. `Songbird::get`
/// alone is not that test: `leave()` keeps the `Call` registered, so it
/// answers "yes" forever after the first empty-channel teardown and every
/// later `/login` would skip the re-join.
pub(super) async fn bot_in_voice(manager: &songbird::Songbird, guild_id: GuildId) -> bool {
    match manager.get(guild_id) {
        Some(call) => call.lock().await.current_channel().is_some(),
        None => false,
    }
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

    // Follow the user in — unless they're parked in the guild's AFK channel,
    // which is nobody's listening room.
    let user_channel = discord_user_id.and_then(|id| {
        guild_id.to_guild_cached(&ctx).and_then(|guild| {
            let afk = guild.afk_metadata.as_ref().map(|a| a.afk_channel_id);
            guild
                .voice_states
                .get(&UserId::new(id))
                .and_then(|vs| vs.channel_id)
                .filter(|ch| Some(*ch) != afk)
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

        match self.guild_id.set_commands(&ctx, commands::register_commands(self.ytdlp_available)).await {
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
        self.dispatch_interaction(ctx, interaction).await;
    }
}

pub struct DiscordBot {
    client: Client,
    ready_rx: mpsc::Receiver<ReadySignal>,
}

impl DiscordBot {
    // Wiring, not logic: each argument is a distinct process-lifetime
    // dependency built in main, so grouping them would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: Arc<Config>,
        bridge: Arc<AudioBridge>,
        presence_rx: mpsc::UnboundedReceiver<PresenceUpdate>,
        presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
        user_store: Arc<UserStore>,
        history: Option<Arc<crate::history::HistoryStore>>,
        queue_store: Option<Arc<crate::queue_store::QueueStore>>,
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
                                if bot_in_voice(&manager, guild_id).await {
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
            ui_send,
            notice_tx,
            presence_tx,
            join_voice: join_voice.clone(),
            spirc_cmd_tx: spirc_cmd_tx.clone(),
            track_handle,
            dj,
            announce_enabled: announce_enabled.clone(),
            history,
            queue_store: queue_store.clone(),
        });

        // Replay the queue the last process was holding. Restoring never
        // starts playback — the core treats it as bookkeeping only.
        if let Some(store) = &queue_store {
            match store.load() {
                Ok(items) if !items.is_empty() => {
                    tracing::info!(count = items.len(), "restoring the queue from the last run");
                    player.restore_queue(items);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not restore the queue"),
            }
        }

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

