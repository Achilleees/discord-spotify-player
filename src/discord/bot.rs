use super::account::ActiveSession;
use super::commands;
use super::voice_owner::{VoiceLease, VoiceOwner};
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// The play history, for the `/history` listing. `None` when the table
    /// could not be opened — the listing then says so instead of failing.
    pub(super) history: Option<Arc<crate::history::HistoryStore>>,
    pub(super) oauth: Arc<SpotifyOAuth>,
    /// Pending device-code pairings keyed by Discord user; notifying cancels
    /// that user's poll. In-memory only: a restart drops pending pairings and
    /// the user re-runs `/login`.
    pub(super) pending_auth: Arc<Mutex<HashMap<u64, Arc<tokio::sync::Notify>>>>,
    pub(super) active_session: Arc<Mutex<Option<ActiveSession>>>,
    /// Set just before the bot removes itself from voice on purpose.
    /// Discord echoes that removal back as a voice-state update, and the
    /// own-disconnect branch below would otherwise read it as a force
    /// disconnect and tear down the Spotify session and the account.
    pub(super) leaving_voice: Arc<AtomicUsize>,
    /// Owns the Spotify session lifecycle (librespot task, refresher, token
    /// state, generation) independently of playback — see
    /// `spotify::session`. Reached from the account operations
    /// (`switch_active_session`, `auto_start_stored_session`,
    /// `handle_logout`, `teardown_playback_session`) and, for `ensure_session`
    /// only, straight from `/play` and `/queue`'s Spotify-link branches in
    /// `commands.rs` — that on-demand bring-up runs in the interaction
    /// handler's own task, never behind an account operation.
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
    pub(super) voice_owner: Arc<VoiceOwner>,
    pub(super) boot: String,
    pub(super) pairings: Mutex<super::routing::Pairings>,
    pub(super) front_menus: Mutex<super::front::Menus>,
    pub(super) pairing_slots: Arc<tokio::sync::Semaphore>,
    pub(super) ytdlp_available: bool,
    pub(super) announce_enabled: Arc<AtomicBool>,
    /// Last /play per user, for the metadata-probe cooldown.
    pub(super) play_cooldowns: Arc<Mutex<HashMap<u64, Instant>>>,
    pub(super) search_menus: Mutex<super::search::SearchMenus>,
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

#[allow(clippy::too_many_arguments)]
async fn play_join_sound_then_bridge(
    owner: Arc<VoiceOwner>,
    lease: VoiceLease,
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
        if owner
            .with_current(&lease, || call.play_only(input.into()))
            .is_none()
        {
            return;
        }
    }

    tokio::select! {
        _ = lease.cancelled.cancelled() => return,
        _ = tokio::time::sleep(std::time::Duration::from_secs_f64(duration_secs + 0.1)) => {}
    }

    let reader = SimpleBridgeReader::new(bridge, prebuffer_samples, prebuffer_wait);
    let input = reader.into_input();
    let mut call = call_lock.lock().await;
    let Some(track_handle) = owner.with_current(&lease, || call.play_only(input.into())) else {
        return;
    };
    let _ = track_handle.add_event(Event::Track(TrackEvent::Error), TrackErrorHandler);
    let _ = track_handle.add_event(Event::Track(TrackEvent::End), TrackErrorHandler);
    tracing::info!(track_uuid = ?track_handle.uuid(), "bridge reader connected after join sound");
    let mut lock = track_handle_store.lock();
    owner.with_current(&lease, || *lock = Some(track_handle));
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
/// Take one outstanding deliberate departure off the guard, reporting
/// whether there was one to take. Counting rather than flagging is what keeps
/// two departures from interfering: each arming is undone or consumed exactly
/// once, so a second `/stop` racing the first one's gateway echo can no
/// longer clear the first one's arming and turn that echo into a phantom
/// force disconnect.
pub(super) fn consume_deliberate_leave(guard: &AtomicUsize) -> bool {
    guard
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

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
    owner: Arc<VoiceOwner>,
    lease: VoiceLease,
    ctx_store: Arc<Mutex<Option<Context>>>,
    guild_id: GuildId,
    bridge: Arc<AudioBridge>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
    dj: Arc<DJAnnouncer>,
) -> bool {
    let ctx = {
        let lock = ctx_store.lock();
        match lock.clone() {
            Some(c) => c,
            None => {
                owner.failed(&lease);
                tracing::warn!("no ctx available for voice join");
                return false;
            }
        }
    };

    let _transition = owner.transitions.lock().await;
    if !owner.current(&lease) {
        return false;
    }
    let target_channel = ChannelId::new(lease.channel);
    let manager = match songbird::get(&ctx).await {
        Some(m) => m,
        None => {
            owner.failed(&lease);
            return false;
        }
    };
    if owner.connected(&lease) {
        if let Some(call) = manager.get(guild_id) {
            if call.lock().await.current_channel().map(|ch| ch.0.get()) == Some(lease.channel) {
                return owner.current(&lease);
            }
        }
    }

    match manager.join(guild_id, target_channel).await {
        Ok(call) => {
            if !owner.mark_connected(&lease) {
                return false;
            }
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
            tokio::spawn(play_join_sound_then_bridge(
                owner.clone(),
                lease.clone(),
                call,
                bridge,
                prebuffer_samples,
                prebuffer_wait,
                track_handle_store,
                dj,
            ));
            owner.current(&lease)
        }
        Err(e) => {
            owner.failed(&lease);
            tracing::warn!(error = ?e, "failed to join voice channel");
            false
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "discord bot connected");

        let registered = match self.config.routing.mode {
            crate::routing::CommandMode::Standalone => {
                let mut commands = commands::register_commands(self.ytdlp_available);
                commands.extend(super::admin::register_commands(self.config.profile));
                commands
            }
            crate::routing::CommandMode::Coordinator => super::front::register_commands(),
            crate::routing::CommandMode::Worker => Vec::new(),
        };
        match self.guild_id.set_commands(&ctx, registered).await {
            Ok(cmds) => tracing::info!("registered {} slash commands", cmds.len()),
            Err(e) => tracing::warn!(error = ?e, "failed to register slash commands"),
        }

        if self.config.routing.mode != crate::routing::CommandMode::Standalone {
            // Guild registration does not replace global commands. Remove only
            // this application's known legacy slash commands during cutover.
            match serenity::all::Command::get_global_commands(&ctx.http).await {
                Err(error) => tracing::warn!(?error, "could not inspect legacy global commands"),
                Ok(commands) => for command in commands {
                    if command.kind == serenity::all::CommandType::ChatInput && matches!(command.name.as_str(),
                        "login"|"logout"|"forget"|"who"|"queue"|"skip"|"stop"|"clear"|"np"|"history"|"announce"|"play"|"slowmode"|"purge"|"music"|"server") {
                        if let Err(error) = serenity::all::Command::delete_global_command(&ctx.http, command.id).await {
                            tracing::warn!(?error, "could not remove a legacy global command");
                        }
                    }
                }
            }
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
            let tx = ui::spawn(ctx.clone(), self.text_channel_id, self.ytdlp_available);
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
            if let Some(channel) = new.channel_id {
                self.voice_owner.observe_channel(channel.get());
            } else {
                // Our own `/stop` leave: expected, and it must not touch the
                // session or the account.
                if consume_deliberate_leave(&self.leaving_voice) {
                    tracing::debug!("bot left voice deliberately — no teardown");
                    return;
                }
                // Not ours, so the core has to hear about it. The teardown
                // below sends `VoiceLost` itself, but it only runs when a
                // session or playback is live — and a bot dragged out after a
                // YouTube item finished, with nobody logged in, satisfies
                // neither. The core would go on believing voice was `Ready`,
                // `ensure_voice` would return early, and the next `/play`
                // would feed a bridge no call drains.
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
                } else {
                    tracing::info!("bot disconnected from voice with nothing playing");
                    self.voice_owner.retire();
                    self.player.send(PlayerInput::VoiceLost);
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
        if humans_in_bot_channel == 0
            && self.voice_owner.snapshot().1 == bot_channel.map(|channel| channel.get())
        {
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

        let voice_owner = Arc::new(VoiceOwner::default());
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
            let owner = voice_owner.clone();
            Arc::new(
                move |discord_user_id, guard: Option<crate::player::state::VoiceGuard>| {
                    // Claim synchronously when the actor emits the effect, before
                    // its spawned task can race a later Stop or another room.
                    let target = owner
                        .snapshot()
                        .1
                        .or_else(|| {
                            let ctx = ctx_store.lock().clone()?;
                            let guild = guild_id.to_guild_cached(&ctx)?;
                            let afk = guild.afk_metadata.as_ref().map(|a| a.afk_channel_id);
                            discord_user_id
                                .and_then(|id| guild.voice_states.get(&UserId::new(id)))
                                .and_then(|vs| vs.channel_id)
                                .filter(|ch| Some(*ch) != afk)
                                .map(|ch| ch.get())
                        })
                        .unwrap_or(channel_id.get());
                    let user_still_here = guard.as_ref().is_none_or(|guard| {
                        let Some(ctx) = ctx_store.lock().clone() else {
                            return false;
                        };
                        let Some(guild) = guild_id.to_guild_cached(&ctx) else {
                            return false;
                        };
                        let room = guild
                            .voice_states
                            .get(&UserId::new(guard.user))
                            .and_then(|vs| vs.channel_id)
                            .map(|ch| ch.get());
                        room == Some(guard.room) && target == guard.room
                    });
                    let lease = user_still_here
                        .then(|| owner.claim_for(target, guard.as_ref().map(|g| g.generation)))
                        .flatten();
                    let owner = owner.clone();
                    let ctx_store = ctx_store.clone();
                    let bridge = bridge.clone();
                    let track_handle = track_handle.clone();
                    let dj = dj.clone();
                    Box::pin(async move {
                        let (lease, revision) = lease?;
                        join_voice_inner(
                            owner,
                            lease,
                            ctx_store,
                            guild_id,
                            bridge,
                            prebuffer_samples,
                            prebuffer_wait,
                            track_handle,
                            dj,
                        )
                        .await
                        .then_some(revision)
                    })
                },
            )
        };

        // Deliberate departure (`/stop`). Sets the guard first so the
        // gateway echo of our own removal isn't read as a force disconnect.
        // `remove`, not `leave`: leave keeps the Call registered and every
        // later presence check would read it as "still in a call".
        let leaving_voice = Arc::new(AtomicUsize::new(0));
        let leave_voice: player_actor::LeaveVoiceFn = {
            let owner = voice_owner.clone();
            let ctx_store = ctx_store.clone();
            let leaving_voice = leaving_voice.clone();
            Arc::new(move || -> Pin<Box<dyn Future<Output = ()> + Send>> {
                let retirement = owner.retire();
                let owner = owner.clone();
                let ctx_store = ctx_store.clone();
                let leaving_voice = leaving_voice.clone();
                Box::pin(async move {
                    let _transition = owner.transitions.lock().await;
                    if !owner.retirement_current(retirement) { return; }
                    let Some(ctx) = ({ ctx_store.lock().clone() }) else { return };
                    let Some(manager) = songbird::get(&ctx).await else { return };
                    leaving_voice.fetch_add(1, Ordering::SeqCst);
                    let left = manager.remove(guild_id).await;
                    if let Err(e) = left {
                        // Nothing was removed (already out of the channel),
                        // so Discord sends no voice-state update and nothing
                        // would ever consume this arming. Left standing, it
                        // would swallow the NEXT genuine force-disconnect and
                        // leave librespot feeding a dead call.
                        //
                        // It is a count rather than a flag so that undoing
                        // can only ever undo *this* departure: with a shared
                        // bool, a second `/stop` racing the first one's echo
                        // cleared the first one's arming, and that echo then
                        // read as a force disconnect and tore the session
                        // down.
                        consume_deliberate_leave(&leaving_voice);
                        tracing::debug!(error = ?e, "leave was a no-op — not already in voice");
                        return;
                    }
                    tracing::info!("bot left voice channel");
                })
            })
        };

        let notice_tx = spawn_notice_task(ctx_store.clone(), text_channel_id);

        let authorize_voice = {
            let owner = voice_owner.clone();
            let ctx_store = ctx_store.clone();
            Arc::new(move |guard: &crate::player::state::VoiceGuard| {
                let Some(ctx) = ctx_store.lock().clone() else {
                    return false;
                };
                let Some(guild) = guild_id.to_guild_cached(&ctx) else {
                    return false;
                };
                let user_room = guild
                    .voice_states
                    .get(&UserId::new(guard.user))
                    .and_then(|vs| vs.channel_id)
                    .map(|ch| ch.get());
                let (generation, claimed) = owner.snapshot();
                let bot_room = claimed.or_else(|| guild.voice_states.get(&ctx.cache.current_user().id).and_then(|vs| vs.channel_id).map(|ch| ch.get()));
                // Fresh joins must follow a listening room. The join helper
                // excludes AFK; do not let a human request fall back elsewhere.
                let afk = guild.afk_metadata.as_ref().map(|a| a.afk_channel_id.get());
                let user_room = user_room.filter(|room| Some(*room) != afk || bot_room == Some(*room));
                guard.allows(generation, bot_room, user_room)
            })
        };
        let player = player_actor::spawn(PlayerDeps {
            authorize_voice,
            bridge: bridge.clone(),
            ui_send,
            notice_tx,
            presence_tx,
            join_voice: join_voice.clone(),
            leave_voice,
            spirc_cmd_tx: spirc_cmd_tx.clone(),
            track_handle,
            dj,
            announce_enabled: announce_enabled.clone(),
            history: history.clone(),
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
            leaving_voice,
            history: history.clone(),
            supervisor,
            ctx: ctx_store,
            ui_tx: ui_tx_store,
            player,
            join_voice,
            voice_owner,
            boot: uuid::Uuid::new_v4().to_string(),
            pairings: Mutex::new(super::routing::Pairings::default()),
            front_menus: Mutex::new(super::front::Menus::default()),
            pairing_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            ytdlp_available,
            announce_enabled,
            play_cooldowns: Arc::new(Mutex::new(HashMap::new())),
            search_menus: Mutex::new(super::search::SearchMenus::default()),
            auto_start_attempted: AtomicBool::new(false),
        };

        let handler = Arc::new(handler);
        if config.routing.mode != crate::routing::CommandMode::Standalone {
            let executor: crate::routing::transport::Executor = {
                let handler = handler.clone();
                Arc::new(move |request| {
                    let handler = handler.clone();
                    Box::pin(async move { handler.execute_routed(request).await })
                })
            };
            crate::routing::transport::listen(
                config.routing.listen.unwrap(),
                config.routing.key.unwrap(),
                executor,
            )
            .await?;
        }
        let token = config.discord_token.clone();
        let client = Client::builder(&token, intents)
            .event_handler_arc(handler)
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
