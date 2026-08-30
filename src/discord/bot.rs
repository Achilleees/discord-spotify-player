use super::presence::run_presence_loop;
use super::ui::{self, CardView, HistoryView, UiMsg};
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio::generate_join_sound;
use crate::audio::dj::DJAnnouncer;
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::{DeviceAuthorization, SpotifyOAuth};
use crate::player::actor::{self as player_actor, PlayerDeps, PlayerHandle, UiEvent};
use crate::player::state::{EnqueuePos, Input as PlayerInput, TrackMeta, TransportEvent};
use crate::presence::PresenceUpdate;
use crate::queue::{PriorityQueue, QueueItem, MediaSource};
use crate::spotify::SpotifyPlayer;
use crate::spotify::SpircCommand;
use librespot_core::SpotifyUri;
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
pub(crate) struct SpotifyTrackInfo {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album_art_url: Option<String>,
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
    /// The UI task's mailbox. `None` until `ready()`'s first pass spawns
    /// the task (see `ui::spawn`); every send site resolves it fresh via
    /// `.lock().clone()`, mirroring `ctx` above.
    ui_tx: Arc<Mutex<Option<mpsc::UnboundedSender<UiMsg>>>>,
    /// The player actor's mailbox: every playback-affecting command (/play,
    /// /queue media, /skip, /stop, ⏯, ⏮, /np) and event (transport, media
    /// end, voice) funnels through it, so decide-then-act is serialized.
    player: PlayerHandle,
    // YouTube/file playback fields
    ytdlp_available: bool,
    priority_queue: Arc<Mutex<PriorityQueue>>,
    spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    /// The media item currently being fed, written by the actor's media
    /// runner. C3-transitional: the presence loop and the teardown checks
    /// still read it; the actor's own `Active::Media` supersedes it in C5.
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    feeder_cancel: Arc<Mutex<Option<CancellationToken>>>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    /// Metadata of the current Spotify track, kept fresh by the presence
    /// loop so /np can answer for the Spotify baseline too.
    last_spotify_meta: Arc<Mutex<Option<SpotifyTrackInfo>>>,
    /// The Spotify baseline session's playback state, kept fresh by the
    /// presence loop so /queue, the queue listing, and the Spotify enqueue
    /// path can read it without a Web API call.
    spotify_state: Arc<Mutex<SpotifyState>>,
    /// Last /play per user, for the metadata-probe cooldown.
    play_cooldowns: Arc<Mutex<HashMap<u64, Instant>>>,
    auto_start_attempted: AtomicBool,
    /// The Spotify track (if any) already pre-armed via `AddToQueue` —
    /// found anywhere in `priority_queue`, not just at the head — for
    /// gap-free radio-rules handoff. See `try_arm_first_spotify`/
    /// `head_action`. `None` when nothing is armed.
    armed_spotify: Arc<Mutex<Option<SpotifyUri>>>,
}

/// The Spotify Connect baseline session's playback state, as last reported by
/// a `PresenceUpdate`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpotifyState {
    Idle,
    Playing,
    Paused,
}

// --- Unified-queue head decision table (see PORT.md decision #15) ---

/// What sits at the head of the priority queue: a Spotify track (and
/// whether it's already pre-armed via `AddToQueue`), a media (YouTube/file)
/// item, or nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HeadKind {
    Spotify { armed: bool },
    Media,
    Empty,
}

/// What provoked a `head_action`/`Handler::reconcile` check.
/// Only `Enqueue` and `PlayButton` are still produced by live code — the
/// player actor owns the skip/track-end/media-end boundaries now — but the
/// full decision table (and its tests) stays intact until reconcile
/// dissolves into the actor in C5.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trigger {
    /// A `/play` or `/queue` push landed a new item (possibly at the head).
    Enqueue,
    /// The Spotify baseline's own `EndOfTrack` fired.
    TrackEnd,
    /// A priority-queue drain (YouTube/file item) just finished.
    MediaEnd,
    /// ⏭ / `/skip` with no media item actively playing.
    Skip,
    /// ▶ pressed while the Spotify baseline isn't playing.
    PlayButton,
}

/// What `Handler::reconcile` should do about the current queue head.
///
/// "Radio rules": tracks play in strict bot-queue order regardless of
/// source, the bot never sends `SpircCommand::Next` except on an explicit
/// user skip, and Spotify never plays over an active media item. The
/// mechanism is arming the *first* Spotify item anywhere in the queue (not
/// just the head) once Spotify is confirmed playing: `AddToQueue` puts it
/// on librespot's own auto-advance, which then lands on it whenever
/// whatever's ahead of it (in our queue, media or otherwise) finishes —
/// paused at 0:00 if a media item got there first, since media playback
/// pauses the Spotify baseline while it runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HeadAction {
    /// Pre-arm the first Spotify item anywhere in the queue via
    /// `AddToQueue` (see `try_arm_first_spotify`).
    Arm,
    /// Queue the first un-armed Spotify item behind Spotify's current
    /// context (`AddToQueue`, no `Next`) and mark it armed (see
    /// `queue_behind_current`). Never pops — its own `Playing` event does.
    QueueBehindCurrent,
    /// `QueueBehindCurrent`, then resume the Spotify baseline (`Play`) —
    /// only if it was playing before whatever just triggered this.
    QueueThenResume,
    /// `QueueBehindCurrent`, then skip straight to it (`Next`).
    QueueThenNext,
    /// `Load` the head now (Spotify is idle — no context to lose).
    Load,
    /// Run/continue a priority-queue drain of the media item at the head.
    Drain,
    /// Send `SpircCommand::Next` (so Spotify also leaves whatever it's on),
    /// then run/continue a drain of the media item at the head.
    NextThenDrain,
    /// Resume the Spotify baseline (`Play`).
    ResumeSpotify,
    /// Skip to the already-armed track (`Next`).
    NextSpotify,
    /// Nothing to do.
    Nothing,
}

/// Pure decision table mapping the queue head, the Spotify baseline's
/// playback state, whether a media item is mid-playback, and what triggered
/// the check, to the action to take. This is the single place unified-queue
/// ordering behaviour is decided — every playback-affecting command and
/// event funnels through it via `Handler::reconcile` (or the free-fn
/// equivalents used where `&self` isn't available).
fn head_action(head: HeadKind, spotify: SpotifyState, media_active: bool, trigger: Trigger) -> HeadAction {
    use HeadAction::*;
    match (trigger, head) {
        // Arm is a no-op (via try_arm_first_spotify) when there's nothing
        // un-armed to arm, so this doesn't need to inspect the head at all.
        (Trigger::Enqueue, _) => {
            if spotify == SpotifyState::Playing && !media_active { Arm } else { Nothing }
        }

        (Trigger::TrackEnd, _) if media_active => Nothing,
        (Trigger::TrackEnd, HeadKind::Spotify { armed: true }) => Nothing,
        (Trigger::TrackEnd, HeadKind::Spotify { armed: false }) => QueueBehindCurrent,
        (Trigger::TrackEnd, HeadKind::Media) => Drain,
        (Trigger::TrackEnd, HeadKind::Empty) => Nothing,

        (Trigger::MediaEnd, HeadKind::Spotify { armed: true }) => ResumeSpotify,
        (Trigger::MediaEnd, HeadKind::Spotify { armed: false }) => {
            if spotify == SpotifyState::Idle { Load } else { QueueThenResume }
        }
        (Trigger::MediaEnd, HeadKind::Media) => Nothing,
        (Trigger::MediaEnd, HeadKind::Empty) => Nothing,

        (Trigger::Skip, HeadKind::Spotify { armed: true }) => NextSpotify,
        (Trigger::Skip, HeadKind::Spotify { armed: false }) => QueueThenNext,
        (Trigger::Skip, HeadKind::Media) => NextThenDrain,
        (Trigger::Skip, HeadKind::Empty) => NextSpotify,

        (Trigger::PlayButton, HeadKind::Spotify { armed: true }) => ResumeSpotify,
        (Trigger::PlayButton, HeadKind::Spotify { armed: false }) => {
            if spotify == SpotifyState::Idle { Load } else { QueueThenResume }
        }
        (Trigger::PlayButton, HeadKind::Media) => Drain,
        (Trigger::PlayButton, HeadKind::Empty) => ResumeSpotify,
    }
}

#[cfg(test)]
mod head_action_tests {
    use super::{head_action, HeadAction, HeadKind, SpotifyState, Trigger};

    const PLAYING: SpotifyState = SpotifyState::Playing;
    const PAUSED: SpotifyState = SpotifyState::Paused;
    const IDLE: SpotifyState = SpotifyState::Idle;
    const SPOT_ARMED: HeadKind = HeadKind::Spotify { armed: true };
    const SPOT_UNARMED: HeadKind = HeadKind::Spotify { armed: false };

    #[test]
    fn enqueue_arms_while_spotify_is_playing_regardless_of_head() {
        // Arm looks anywhere in the queue for the first un-armed Spotify
        // item (and is a no-op if there isn't one), so this trigger doesn't
        // need to inspect the head kind at all.
        assert_eq!(head_action(SPOT_UNARMED, PLAYING, false, Trigger::Enqueue), HeadAction::Arm);
        assert_eq!(head_action(SPOT_ARMED, PLAYING, false, Trigger::Enqueue), HeadAction::Arm);
        assert_eq!(head_action(HeadKind::Media, PLAYING, false, Trigger::Enqueue), HeadAction::Arm);
        assert_eq!(head_action(HeadKind::Empty, PLAYING, false, Trigger::Enqueue), HeadAction::Arm);
    }

    #[test]
    fn enqueue_never_arms_while_media_is_active() {
        assert_eq!(head_action(SPOT_UNARMED, PLAYING, true, Trigger::Enqueue), HeadAction::Nothing);
    }

    #[test]
    fn enqueue_does_nothing_unless_spotify_is_playing() {
        assert_eq!(head_action(SPOT_UNARMED, PAUSED, false, Trigger::Enqueue), HeadAction::Nothing);
        assert_eq!(head_action(SPOT_UNARMED, IDLE, false, Trigger::Enqueue), HeadAction::Nothing);
    }

    #[test]
    fn track_end_leaves_an_armed_spotify_head_alone() {
        assert_eq!(head_action(SPOT_ARMED, PLAYING, false, Trigger::TrackEnd), HeadAction::Nothing);
    }

    #[test]
    fn track_end_queues_behind_current_for_an_unarmed_spotify_head() {
        assert_eq!(head_action(SPOT_UNARMED, PAUSED, false, Trigger::TrackEnd), HeadAction::QueueBehindCurrent);
    }

    #[test]
    fn track_end_drains_a_media_head() {
        assert_eq!(head_action(HeadKind::Media, PLAYING, false, Trigger::TrackEnd), HeadAction::Drain);
    }

    #[test]
    fn track_end_does_nothing_while_media_is_active_racing_a_drain() {
        assert_eq!(head_action(SPOT_UNARMED, PLAYING, true, Trigger::TrackEnd), HeadAction::Nothing);
        assert_eq!(head_action(HeadKind::Media, PLAYING, true, Trigger::TrackEnd), HeadAction::Nothing);
    }

    #[test]
    fn track_end_does_nothing_on_an_empty_queue() {
        assert_eq!(head_action(HeadKind::Empty, PLAYING, false, Trigger::TrackEnd), HeadAction::Nothing);
    }

    #[test]
    fn media_end_resumes_an_armed_spotify_head() {
        assert_eq!(head_action(SPOT_ARMED, PAUSED, false, Trigger::MediaEnd), HeadAction::ResumeSpotify);
    }

    #[test]
    fn media_end_queues_behind_current_for_an_unarmed_spotify_head_when_not_idle() {
        assert_eq!(head_action(SPOT_UNARMED, PAUSED, false, Trigger::MediaEnd), HeadAction::QueueThenResume);
    }

    #[test]
    fn media_end_loads_an_unarmed_spotify_head_while_idle() {
        assert_eq!(head_action(SPOT_UNARMED, IDLE, false, Trigger::MediaEnd), HeadAction::Load);
    }

    #[test]
    fn media_end_does_nothing_on_an_empty_queue() {
        assert_eq!(head_action(HeadKind::Empty, PLAYING, false, Trigger::MediaEnd), HeadAction::Nothing);
    }

    #[test]
    fn skip_jumps_to_an_armed_spotify_head() {
        assert_eq!(head_action(SPOT_ARMED, PLAYING, false, Trigger::Skip), HeadAction::NextSpotify);
    }

    #[test]
    fn skip_queues_behind_current_then_skips_an_unarmed_spotify_head() {
        assert_eq!(head_action(SPOT_UNARMED, PLAYING, false, Trigger::Skip), HeadAction::QueueThenNext);
    }

    #[test]
    fn skip_sends_next_then_drains_a_media_head() {
        assert_eq!(head_action(HeadKind::Media, PLAYING, false, Trigger::Skip), HeadAction::NextThenDrain);
    }

    #[test]
    fn skip_on_an_empty_queue_skips_the_spotify_baseline() {
        assert_eq!(head_action(HeadKind::Empty, PLAYING, false, Trigger::Skip), HeadAction::NextSpotify);
    }

    #[test]
    fn play_button_resumes_an_armed_spotify_head() {
        assert_eq!(head_action(SPOT_ARMED, PAUSED, false, Trigger::PlayButton), HeadAction::ResumeSpotify);
    }

    #[test]
    fn play_button_loads_an_unarmed_spotify_head_while_idle() {
        assert_eq!(head_action(SPOT_UNARMED, IDLE, false, Trigger::PlayButton), HeadAction::Load);
    }

    #[test]
    fn play_button_never_hijacks_a_paused_baseline() {
        assert_eq!(head_action(SPOT_UNARMED, PAUSED, false, Trigger::PlayButton), HeadAction::QueueThenResume);
    }

    #[test]
    fn play_button_drains_a_media_head() {
        assert_eq!(head_action(HeadKind::Media, PAUSED, false, Trigger::PlayButton), HeadAction::Drain);
    }

    #[test]
    fn play_button_on_an_empty_queue_resumes_the_baseline() {
        assert_eq!(head_action(HeadKind::Empty, PAUSED, false, Trigger::PlayButton), HeadAction::ResumeSpotify);
    }
}

/// Classifies the current head of `priority_queue` into a `HeadKind`,
/// comparing a Spotify head's URI against `armed` to decide `armed: bool`
/// (whether this particular head item is the one currently armed — armed
/// itself can sit anywhere else in the queue). Locks `armed` first, then
/// `priority_queue` — same order as `try_arm_first_spotify`/
/// `queue_behind_current`, and never holds both at once.
fn classify_head(priority_queue: &Mutex<PriorityQueue>, armed: &Mutex<Option<SpotifyUri>>) -> HeadKind {
    let armed_uri = { armed.lock().clone() };
    let lock = priority_queue.lock();
    match lock.peek() {
        None => HeadKind::Empty,
        Some(item) => match &item.source {
            MediaSource::Spotify { uri, .. } => HeadKind::Spotify { armed: armed_uri.as_ref() == Some(uri) },
            _ => HeadKind::Media,
        },
    }
}

/// Finds the first Spotify item anywhere in `priority_queue` and returns
/// its URI. Shared by `try_arm_first_spotify` and `queue_behind_current` —
/// both only ever call this while `armed` is confirmed `None`, so "first
/// Spotify item" and "first un-armed Spotify item" coincide. Caller must
/// already hold `priority_queue`'s lock (passed in as `lock` to avoid
/// re-entrant locking).
fn first_spotify_uri(lock: &PriorityQueue) -> Option<SpotifyUri> {
    lock.find_first(|item| matches!(item.source, MediaSource::Spotify { .. }))
        .and_then(|item| match &item.source {
            MediaSource::Spotify { uri, .. } => Some(uri.clone()),
            _ => None,
        })
}

/// The armed-head critical section (invariant: an armed track is never
/// `AddToQueue`'d again). If nothing is currently armed, Spotify is
/// playing, and no media item is mid-playback, arms the first Spotify item
/// anywhere in the queue: `AddToQueue`s it onto Spotify's own device queue
/// and remembers it as armed, so librespot's auto-advance lands on it once
/// everything ahead of it in our queue (media items included) is done.
/// Returns whether it armed something.
///
/// Lock order: `armed` first, then `priority_queue` — callers must not
/// already hold `priority_queue` when calling this.
fn try_arm_first_spotify(
    priority_queue: &Mutex<PriorityQueue>,
    armed: &Mutex<Option<SpotifyUri>>,
    spirc_tx: Option<&mpsc::UnboundedSender<SpircCommand>>,
    spotify: SpotifyState,
    media_active: bool,
) -> bool {
    let mut armed_lock = armed.lock();
    if armed_lock.is_some() || spotify != SpotifyState::Playing || media_active {
        return false;
    }
    let tx = match spirc_tx {
        Some(tx) => tx,
        None => return false,
    };
    let uri = {
        let queue_lock = priority_queue.lock();
        first_spotify_uri(&queue_lock)
    };
    let uri = match uri {
        Some(u) => u,
        None => return false,
    };
    if tx.send(SpircCommand::AddToQueue(uri.clone())).is_err() {
        return false;
    }
    *armed_lock = Some(uri);
    true
}

/// `HeadAction::QueueBehindCurrent`: finds the first un-armed Spotify item
/// anywhere in the queue and queues it behind whatever Spotify's device is
/// currently on (`AddToQueue`, never `Next` — that's how this never plays
/// over an active media item or interrupts a mid-play Spotify track), then
/// marks it armed so it isn't queued twice. Never pops it: its own
/// `Playing` event does that once librespot's auto-advance reaches it.
/// No-op if something is already armed or there's no live session. Free
/// function (not a `Handler` method) so it's usable from contexts without
/// `&self`, such as the presence loop.
///
/// Lock order: `armed` first, then `priority_queue` — callers must not
/// already hold `priority_queue` when calling this.
fn queue_behind_current(
    priority_queue: &Mutex<PriorityQueue>,
    armed: &Mutex<Option<SpotifyUri>>,
    spirc_tx: Option<&mpsc::UnboundedSender<SpircCommand>>,
) -> bool {
    let mut armed_lock = armed.lock();
    if armed_lock.is_some() {
        return false;
    }
    let tx = match spirc_tx {
        Some(tx) => tx,
        None => return false,
    };
    let uri = {
        let queue_lock = priority_queue.lock();
        first_spotify_uri(&queue_lock)
    };
    let uri = match uri {
        Some(u) => u,
        None => return false,
    };
    if tx.send(SpircCommand::AddToQueue(uri.clone())).is_err() {
        return false;
    }
    *armed_lock = Some(uri);
    true
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

/// Derives the legacy `PresenceUpdate` the still-live presence loop expects
/// from one `TransportEvent`, using `last_meta` (uri → title/artist/art,
/// updated by `Playing`/`TrackChanged`) to fill in what `Paused` events
/// don't carry. Returns `None` for events with no presence meaning. Pure,
/// so the mapping is testable without a session.
fn derive_presence(
    ev: &TransportEvent,
    last_meta: &mut Option<(String, TrackMeta)>,
) -> Option<PresenceUpdate> {
    match ev {
        TransportEvent::Playing { uri, meta } => {
            if let Some(meta) = meta {
                *last_meta = Some((uri.to_string(), meta.clone()));
            }
            let (title, artist, album_art_url) = match (meta, &*last_meta) {
                (Some(m), _) => (m.title.clone(), m.artist.clone(), m.album_art_url.clone()),
                (None, Some((u, m))) if *u == uri.to_string() => {
                    (m.title.clone(), m.artist.clone(), m.album_art_url.clone())
                }
                _ => ("Unknown track".to_string(), "Unknown artist".to_string(), None),
            };
            Some(PresenceUpdate::Playing { title, artist, track_id: uri.to_id(), album_art_url })
        }
        TransportEvent::Paused { uri } => {
            let (title, artist) = match &*last_meta {
                Some((u, m)) if *u == uri.to_string() => (m.title.clone(), m.artist.clone()),
                _ => ("Unknown track".to_string(), "Unknown artist".to_string()),
            };
            Some(PresenceUpdate::Paused { title, artist, track_id: uri.to_id() })
        }
        TransportEvent::Stopped | TransportEvent::EndOfTrack | TransportEvent::Unavailable { .. } => {
            Some(PresenceUpdate::Idle)
        }
        TransportEvent::TrackChanged { uri, meta } => {
            *last_meta = Some((uri.to_string(), meta.clone()));
            None
        }
        TransportEvent::SetQueue { .. }
        | TransportEvent::SessionConnected
        | TransportEvent::SessionDisconnected => None,
    }
}

/// C3 transport shim: one event stream out of the librespot session, two
/// consumers — the legacy presence loop (via the derived `PresenceUpdate`)
/// and the player actor (via `Input::Transport`). The generation is fixed
/// at 0 until the session supervisor (C4) stamps real link generations;
/// the actor's `link_gen` also starts at 0, so every event is current.
/// Exits when the session task (the only sender) dies.
async fn transport_shim(
    mut rx: mpsc::UnboundedReceiver<TransportEvent>,
    presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
    player: PlayerHandle,
) {
    let mut last_meta: Option<(String, TrackMeta)> = None;
    while let Some(ev) = rx.recv().await {
        if let Some(update) = derive_presence(&ev, &mut last_meta) {
            let _ = presence_tx.send(update);
        }
        player.send(PlayerInput::Transport { gen: 0, ev });
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

#[allow(clippy::too_many_arguments)]
async fn run_presence_loop_with_track(
    ctx: Context,
    mut rx: mpsc::UnboundedReceiver<PresenceUpdate>,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    ui_tx: Option<mpsc::UnboundedSender<UiMsg>>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    bridge: Arc<AudioBridge>,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    last_meta_store: Arc<Mutex<Option<SpotifyTrackInfo>>>,
    spotify_state: Arc<Mutex<SpotifyState>>,
    priority_queue: Arc<Mutex<PriorityQueue>>,
    armed_spotify: Arc<Mutex<Option<SpotifyUri>>>,
    spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
) {
    let (fwd_tx, fwd_rx) = mpsc::unbounded_channel::<PresenceUpdate>();
    let ctx_presence = ctx.clone();
    tokio::spawn(async move {
        run_presence_loop(ctx_presence, fwd_rx).await;
    });

    let mut last_track_key: Option<String> = None;
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
            if let Some(tx) = &ui_tx {
                let _ = tx.send(UiMsg::Buttons { paused: is_paused });
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

        // Armed-head bookkeeping: on every Playing event, if it matches the
        // track we pre-armed (librespot's auto-advance landed on it — it
        // can be anywhere in the queue, not just the head, hence
        // `remove_first` rather than a head-only pop), remove it from our
        // queue and clear armed. Then: if a media item is actively playing,
        // Spotify must not play over it under radio rules — pause it back
        // down (the actor's media-end boundary resumes it once the queue
        // clears);
        // otherwise try to arm whatever the first un-armed Spotify item now
        // is, so chained Spotify items (and a DJ resuming a paused queue
        // from their phone) stay gap-free. Idle means Spotify's own device
        // queue is gone, so nothing is armed anymore.
        // Only `Playing` needs bookkeeping here. `Idle` deliberately does
        // not clear the armed track: librespot emits it at every track
        // boundary (EndOfTrack), so clearing it here would forget a track
        // already sitting in Spotify's queue and queue it a second time.
        // Session teardown (login/logout/forget/stop) clears it instead.
        if let PresenceUpdate::Playing { track_id, .. } = &update {
            {
                // Spotify reporting a track we hold is authoritative: it is
                // playing, so it leaves our queue — whether or not it is
                // still the armed one. The pop must not depend on that
                // bookkeeping surviving, since a missed pop leaves the item
                // queued and it gets handed to Spotify again, playing twice.
                let mut armed_lock = armed_spotify.lock();
                if armed_lock.as_ref().map(|u| u.to_id()).as_deref() == Some(track_id.as_str()) {
                    *armed_lock = None;
                }
                let mut lock = priority_queue.lock();
                lock.remove_first(|item| matches!(&item.source, MediaSource::Spotify { uri, .. } if uri.to_id() == *track_id));
            }
            let media_active = { active_priority_item.lock().is_some() };
            let tx = { spirc_cmd_tx.lock().clone() };
            if media_active {
                if let Some(tx) = &tx {
                    let _ = tx.send(SpircCommand::Pause);
                }
                // This Playing never reached the listeners — no card, no
                // history, and no dedup key, so the track's real start
                // (after the queue clears) posts normally.
                continue;
            }
            try_arm_first_spotify(&priority_queue, &armed_spotify, tx.as_ref(), SpotifyState::Playing, media_active);
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

                // Metadata now comes straight from the librespot session
                // (the PresenceUpdate); there is no Web API fallback fetch.
                let meta = SpotifyTrackInfo {
                    title: title.clone(),
                    artist: artist.clone(),
                    album_art_url: album_art_url.clone(),
                };

                if let Some(tx) = &ui_tx {
                    let _ = tx.send(UiMsg::NowPlaying(CardView::Spotify {
                        title: title.clone(),
                        artist: artist.clone(),
                        track_id: track_id.clone(),
                        album_art_url: album_art_url.clone(),
                        dj_name: spotify_name,
                    }));
                }

                {
                    let mut lock = last_meta_store.lock();
                    *lock = Some(meta);
                }

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
            // Idle means nothing is loaded — clear the dedup key so the next
            // track always posts a fresh card, and drop the stale metadata so
            // /np and /queue don't describe a track that finished. A pause
            // keeps both: resuming the same track is not a track change and
            // must not repost history or the controls.
            if matches!(update, PresenceUpdate::Idle) {
                last_track_key = None;
                let mut lock = last_meta_store.lock();
                *lock = None;
            }
            // A session that comes up already paused (bot restart mid-pause)
            // never sends Playing, so /np would show nothing — seed it here.
            if let PresenceUpdate::Paused { title, artist, track_id: _ } = &update {
                let mut lock = last_meta_store.lock();
                if lock.is_none() {
                    *lock = Some(SpotifyTrackInfo {
                        title: title.clone(),
                        artist: artist.clone(),
                        album_art_url: None,
                    });
                }
            }
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
            let ctx_presence = ctx.clone();
            let track_handle_store = self.track_handle.clone();
            let active_session = self.active_session.clone();
            let ui_tx = { self.ui_tx.lock().clone() };
            let dj_presence = self.dj.clone();
            let bridge_presence = self.bridge.clone();
            let announce_presence = self.announce_enabled.clone();
            let priority_item = self.active_priority_item.clone();
            let last_meta_store = self.last_spotify_meta.clone();
            let spotify_state = self.spotify_state.clone();
            let priority_queue_presence = self.priority_queue.clone();
            let armed_spotify_presence = self.armed_spotify.clone();
            let spirc_cmd_tx_presence = self.spirc_cmd_tx.clone();
            tokio::spawn(async move {
                run_presence_loop_with_track(
                    ctx_presence, rx, track_handle_store, active_session,
                    ui_tx,
                    dj_presence, announce_presence,
                    bridge_presence, priority_item,
                    last_meta_store, spotify_state,
                    priority_queue_presence, armed_spotify_presence, spirc_cmd_tx_presence,
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
                    session || priority
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
            "skip" => self.player.skip().await,
            "stop" => self.player.stop().await,
            "np" => self.player.query().await,
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

    /// Resolves a Spotify track's title/artist/art through the live session
    /// (`SpircCommand::Lookup`), returning `None` if there is no session or
    /// the lookup itself fails — both cases the caller reports identically.
    async fn lookup_spotify_track(&self, uri: &SpotifyUri) -> Option<(String, String, Option<String>)> {
        let tx = { self.spirc_cmd_tx.lock().clone() }?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if tx.send(SpircCommand::Lookup(uri.clone(), reply_tx)).is_err() {
            return None;
        }
        match reply_rx.await {
            Ok(Some(lookup)) => Some((lookup.title, lookup.artist, lookup.album_art_url)),
            Ok(None) | Err(_) => None,
        }
    }

    /// The Spotify-enqueue decision path (C3-transitional): classifies the
    /// current queue head, looks up `head_action` for `trigger`, executes
    /// it, and returns the reply text (if any) the caller should show the
    /// user. Only the `/play`//`/queue` Spotify branches still call this —
    /// every other command goes through the player actor — and they only
    /// reach the `Enqueue` and `PlayButton` triggers; media starts (the
    /// `Drain` arms) are delegated to the actor. Dissolves into the actor
    /// in C5, when the core owns arm bookkeeping.
    async fn reconcile(&self, trigger: Trigger, _requester_id: Option<u64>) -> Option<String> {
        let head = classify_head(&self.priority_queue, &self.armed_spotify);
        let spotify = *self.spotify_state.lock();
        let media_active = { self.active_priority_item.lock().is_some() };
        let action = head_action(head, spotify, media_active, trigger);
        let spirc_tx = { self.spirc_cmd_tx.lock().clone() };

        match action {
            HeadAction::Nothing => None,
            HeadAction::Arm => {
                let armed = try_arm_first_spotify(&self.priority_queue, &self.armed_spotify, spirc_tx.as_ref(), spotify, media_active);
                if armed {
                    Some("Queued — plays after the current track.".to_string())
                } else {
                    None
                }
            }
            HeadAction::Load => {
                let popped = {
                    let mut lock = self.priority_queue.lock();
                    lock.pop_if(|item| matches!(item.source, MediaSource::Spotify { .. }))
                };
                if let Some(QueueItem { source: MediaSource::Spotify { uri, .. }, .. }) = popped {
                    if let Some(tx) = &spirc_tx {
                        let _ = tx.send(SpircCommand::Load(uri));
                    }
                }
                Some("Playing now on Spotify.".to_string())
            }
            HeadAction::QueueBehindCurrent => {
                queue_behind_current(&self.priority_queue, &self.armed_spotify, spirc_tx.as_ref());
                None
            }
            HeadAction::QueueThenResume => {
                queue_behind_current(&self.priority_queue, &self.armed_spotify, spirc_tx.as_ref());
                // Reached only via PlayButton (the media-end boundary lives
                // in the player actor now) — the button press itself is
                // the explicit request to resume.
                if let Some(tx) = &spirc_tx {
                    let _ = tx.send(SpircCommand::Play);
                }
                None
            }
            HeadAction::QueueThenNext => {
                queue_behind_current(&self.priority_queue, &self.armed_spotify, spirc_tx.as_ref());
                if let Some(tx) = &spirc_tx {
                    let _ = tx.send(SpircCommand::Next);
                }
                None
            }
            HeadAction::Drain => {
                // Media starts belong to the actor. ▶ semantics start a
                // media head when nothing holds the turn — exactly this
                // arm's situation — so poke the actor with a play press and
                // keep this path's own reply.
                let _ = self.player.toggle_pause().await;
                None
            }
            HeadAction::NextThenDrain => {
                if let Some(tx) = &spirc_tx {
                    let _ = tx.send(SpircCommand::Next);
                }
                let _ = self.player.toggle_pause().await;
                None
            }
            HeadAction::ResumeSpotify => {
                if let Some(tx) = &spirc_tx {
                    let _ = tx.send(SpircCommand::Play);
                }
                None
            }
            HeadAction::NextSpotify => {
                if let Some(tx) = &spirc_tx {
                    let _ = tx.send(SpircCommand::Next);
                }
                None
            }
        }
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
            let armed_uri = { self.armed_spotify.lock().clone() };
            lines.push(format!("\nPriority queue ({} item(s)):", pq_snapshot.len()));
            for (i, item) in pq_snapshot.iter().enumerate().take(5) {
                let line = match &item.source {
                    MediaSource::Spotify { uri, title, artist, .. } => {
                        let is_armed = armed_uri.as_ref() == Some(uri);
                        let suffix = if is_armed { " ⏭ next on Spotify" } else { "" };
                        format!("  {}. 🎵 **{}** — {}{}", i + 1, title, artist, suffix)
                    }
                    _ => {
                        let duration = item.source.display_duration()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default();
                        format!("  {}. {}{} — queued by {}", i + 1, item.source.display_title(), duration, item.queued_by)
                    }
                };
                lines.push(line);
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

        // VoiceLost first (mailbox order beats the runner's own cancel
        // report): the actor drops any active media turn and stale-ifies
        // the runner's coming `MediaEnded`, so nothing tries to start the
        // next item into a dead voice connection.
        self.player.send(PlayerInput::VoiceLost);
        self.stop_priority_playback();

        let _ = self.presence_tx.send(PresenceUpdate::Idle);

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
    /// Thin wrapper over `join_voice_inner` (shared with the player actor's
    /// `JoinVoice` effect).
    async fn join_voice_for_user(&self, discord_user_id: Option<u64>) -> bool {
        join_voice_inner(
            self.ctx.clone(),
            self.guild_id,
            self.channel_id,
            self.bridge.clone(),
            self.prebuffer_samples,
            self.prebuffer_wait,
            self.track_handle.clone(),
            self.dj.clone(),
            discord_user_id,
        )
        .await
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
        // The previous session's Spotify-side device queue is gone with it —
        // any armed track never plays, so stop treating it as armed.
        {
            let mut lock = self.armed_spotify.lock();
            *lock = None;
        }

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
            let tx = { self.ui_tx.lock().clone() };
            if let Some(tx) = tx {
                let _ = tx.send(UiMsg::Idle { account: Some(discord_name.clone()) });
            }
        }

        // One transport-event stream out of the librespot session. The shim
        // fans it out to the presence loop (as the legacy `PresenceUpdate`)
        // and the player actor (as `Input::Transport`); it exits when this
        // session's task — the only sender — dies.
        let (transport_tx, transport_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (spirc_tx, spirc_rx) = mpsc::unbounded_channel::<SpircCommand>();

        // Store spirc_cmd_tx
        {
            let mut lock = self.spirc_cmd_tx.lock();
            *lock = Some(spirc_tx);
        }

        tokio::spawn(transport_shim(
            transport_rx,
            self.presence_tx.clone(),
            self.player.clone(),
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
                        &config, bridge.clone(),
                        transport_tx.clone(),
                        access_token,
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
        // immediately instead of deferring. Goes into the same unified
        // priority queue as YouTube/file items (PORT.md decision #15).
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let reply = if !self.has_spotify_session() {
                    "Run `/login` first.".to_string()
                } else {
                    match self.lookup_spotify_track(&spotify_uri).await {
                        None => "Couldn't resolve that Spotify track.".to_string(),
                        Some((title, artist, album_art_url)) => {
                            let item = QueueItem {
                            item_id: 0,
                                source: MediaSource::Spotify { uri: spotify_uri, title, artist, album_art_url },
                                queued_by: discord_name.clone(),
                                queued_by_id: discord_id,
                            };
                            let armed_head = { self.armed_spotify.lock().is_some() };
                            let (accepted, queue_len) = {
                                let mut lock = self.priority_queue.lock();
                                let accepted = if next {
                                    // An armed track is already on Spotify's
                                    // own device queue and can't be
                                    // un-queued — a "next" item lands right
                                    // behind our queue's head instead of
                                    // jumping it, so it still plays before
                                    // the armed track's own turn comes up.
                                    if armed_head { lock.insert(1, item) } else { lock.push_front(item) }
                                } else {
                                    lock.push(item)
                                };
                                (accepted, lock.len())
                            };
                            if !accepted {
                                format!("Queue is full ({} items) — try again once some have played.", queue_len)
                            } else {
                                let trigger = if !self.something_is_playing() { Trigger::PlayButton } else { Trigger::Enqueue };
                                match self.reconcile(trigger, Some(discord_id)).await {
                                    Some(msg) => msg,
                                    None => if next { "Playing next".to_string() } else { format!("Added to queue #{}", queue_len) },
                                }
                            }
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
        // pushes into the shared queue, starts the head when nothing holds
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
            // The Spotify-side device queue dies with the session — an armed
            // track never plays, so it's no longer armed.
            {
                let mut lock = self.armed_spotify.lock();
                *lock = None;
            }
            let _ = self.presence_tx.send(PresenceUpdate::Idle);
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
        // Whatever Spotify session that armed track belonged to is being
        // forgotten — stop treating it as armed.
        {
            let mut lock = self.armed_spotify.lock();
            *lock = None;
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

        let discord_name = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let discord_id = cmd.user.id.get();

        // Spotify track link: goes on the same unified priority queue as
        // YouTube/file items (PORT.md decision #15), no defer needed.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let reply = if !self.has_spotify_session() {
                    "Run `/login` first.".to_string()
                } else {
                    match self.lookup_spotify_track(&spotify_uri).await {
                        None => "Couldn't resolve that Spotify track.".to_string(),
                        Some((title, artist, album_art_url)) => {
                            let item = QueueItem {
                            item_id: 0,
                                source: MediaSource::Spotify { uri: spotify_uri, title, artist, album_art_url },
                                queued_by: discord_name.clone(),
                                queued_by_id: discord_id,
                            };
                            let (accepted, queue_len) = {
                                let mut lock = self.priority_queue.lock();
                                let accepted = lock.push(item);
                                (accepted, lock.len())
                            };
                            if !accepted {
                                format!("Queue is full ({} items) — try again once some have played.", queue_len)
                            } else {
                                match self.reconcile(Trigger::Enqueue, None).await {
                                    Some(msg) => msg,
                                    None => format!("Added to queue #{}", queue_len),
                                }
                            }
                        }
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

        // Shared surfaces the player actor bridges to in C3 (queue and
        // arming stay Handler-owned until C5; see `player::actor`), hoisted
        // so both the actor's deps and the Handler hold the same `Arc`s.
        let guild_id = GuildId::new(config.discord_guild_id);
        let channel_id = ChannelId::new(config.discord_channel_id);
        let text_channel_id = ChannelId::new(config.discord_text_channel_id);
        let ctx_store: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
        let ui_tx_store: Arc<Mutex<Option<mpsc::UnboundedSender<UiMsg>>>> =
            Arc::new(Mutex::new(None));
        let priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
        let armed_spotify: Arc<Mutex<Option<SpotifyUri>>> = Arc::new(Mutex::new(None));
        let spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>> =
            Arc::new(Mutex::new(None));
        let active_priority_item: Arc<Mutex<Option<QueueItem>>> = Arc::new(Mutex::new(None));
        let feeder_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
        let feeder_paused = Arc::new(AtomicBool::new(false));
        // Restore the persisted /announce toggle so restarts (including
        // the VPS updater's) don't silently disable announcements.
        let announce_enabled = Arc::new(AtomicBool::new(
            announce_persisted.as_deref() == Some("1"),
        ));

        // Synchronous UI dispatch for the actor: resolve the UI task's
        // mailbox fresh per send (it exists only after ready()) and map the
        // actor's events onto the UI task's own message type, which is
        // private to this module.
        let ui_send: player_actor::UiSendFn = {
            let ui_tx = ui_tx_store.clone();
            Arc::new(move |event: UiEvent| {
                let tx = { ui_tx.lock().clone() };
                if let Some(tx) = tx {
                    let _ = tx.send(match event {
                        UiEvent::NowPlayingMedia { item } => {
                            UiMsg::NowPlaying(CardView::Queued { item })
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
            presence_tx: presence_tx.clone(),
            join_voice,
            spirc_cmd_tx: spirc_cmd_tx.clone(),
            priority_queue: priority_queue.clone(),
            armed_spotify: armed_spotify.clone(),
            active_priority_item: active_priority_item.clone(),
            track_handle: track_handle.clone(),
            feeder_cancel: feeder_cancel.clone(),
            feeder_paused: feeder_paused.clone(),
            dj: dj.clone(),
            announce_enabled: announce_enabled.clone(),
        });

        let handler = Handler {
            guild_id,
            channel_id,
            text_channel_id,
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
            ctx: ctx_store,
            ui_tx: ui_tx_store,
            player,
            // YouTube/file fields
            ytdlp_available,
            priority_queue,
            spirc_cmd_tx,
            active_priority_item,
            feeder_cancel,
            dj,
            announce_enabled,
            last_spotify_meta: Arc::new(Mutex::new(None)),
            spotify_state: Arc::new(Mutex::new(SpotifyState::Idle)),
            play_cooldowns: Arc::new(Mutex::new(HashMap::new())),
            auto_start_attempted: AtomicBool::new(false),
            armed_spotify,
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
        derive_presence, is_valid_track_id, parse_track_id_from_url, voice_gate, TrackMeta,
        TransportEvent,
    };
    use crate::presence::PresenceUpdate;
    use librespot_core::SpotifyUri;
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

    // --- derive_presence: the transport shim's presence-loop mapping ---

    fn uri() -> SpotifyUri {
        SpotifyUri::from_uri(&format!("spotify:track:{ID}")).unwrap()
    }

    fn meta(title: &str) -> TrackMeta {
        TrackMeta { title: title.into(), artist: "artist".into(), album_art_url: None }
    }

    #[test]
    fn playing_with_meta_maps_and_caches() {
        let mut cache = None;
        let ev = TransportEvent::Playing { uri: uri(), meta: Some(meta("Song")) };
        match derive_presence(&ev, &mut cache) {
            Some(PresenceUpdate::Playing { title, artist, track_id, .. }) => {
                assert_eq!(title, "Song");
                assert_eq!(artist, "artist");
                assert_eq!(track_id, ID);
            }
            other => panic!("expected Playing, got {other:?}"),
        }
        assert!(cache.is_some(), "meta is cached for the next Paused");
    }

    #[test]
    fn paused_reuses_the_cached_meta() {
        let mut cache = None;
        let _ = derive_presence(
            &TransportEvent::Playing { uri: uri(), meta: Some(meta("Song")) },
            &mut cache,
        );
        match derive_presence(&TransportEvent::Paused { uri: uri() }, &mut cache) {
            Some(PresenceUpdate::Paused { title, track_id, .. }) => {
                assert_eq!(title, "Song");
                assert_eq!(track_id, ID);
            }
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[test]
    fn paused_without_history_falls_back_to_unknown() {
        let mut cache = None;
        match derive_presence(&TransportEvent::Paused { uri: uri() }, &mut cache) {
            Some(PresenceUpdate::Paused { title, artist, .. }) => {
                assert_eq!(title, "Unknown track");
                assert_eq!(artist, "Unknown artist");
            }
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[test]
    fn track_changed_caches_without_a_presence_update() {
        let mut cache = None;
        let ev = TransportEvent::TrackChanged { uri: uri(), meta: meta("Song") };
        assert!(derive_presence(&ev, &mut cache).is_none());
        // The cache it fills is what the next Paused renders from.
        match derive_presence(&TransportEvent::Paused { uri: uri() }, &mut cache) {
            Some(PresenceUpdate::Paused { title, .. }) => assert_eq!(title, "Song"),
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[test]
    fn boundary_events_map_to_idle() {
        let mut cache = None;
        for ev in [
            TransportEvent::Stopped,
            TransportEvent::EndOfTrack,
            TransportEvent::Unavailable { uri: uri() },
        ] {
            assert!(
                matches!(derive_presence(&ev, &mut cache), Some(PresenceUpdate::Idle)),
                "{ev:?} should read as Idle"
            );
        }
    }

    #[test]
    fn session_events_have_no_presence_meaning() {
        let mut cache = None;
        for ev in [
            TransportEvent::SetQueue { current: None, queued: vec![] },
            TransportEvent::SessionConnected,
            TransportEvent::SessionDisconnected,
        ] {
            assert!(derive_presence(&ev, &mut cache).is_none(), "{ev:?} should map to None");
        }
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
