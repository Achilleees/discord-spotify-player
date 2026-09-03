//! Pure player-decision core: one owned [`PlayerState`], advanced by [`step`].
//!
//! This file is the bot's notion of playback intent — who is entitled to be
//! audible and what has to happen next — kept free of transport and UI
//! machinery so it ports to nob unchanged. It imports no serenity, songbird
//! or librespot-connect types: only `std`, `tokio::sync::oneshot` (a plain
//! channel handle, not IO), `librespot_core::SpotifyUri` (a plain id) and
//! `crate::queue`. Discord-shaped payloads travel behind local structs
//! ([`TrackMeta`], [`UiMsg`], [`PresenceState`], [`PlayerSnapshot`]).
//!
//! `step` is synchronous and pure: it mutates the state and returns
//! [`Effect`]s for the actor to run. It never sleeps, spawns, logs to
//! Discord or performs IO, and it reads time only from its `now` parameter,
//! so every behaviour here is deterministic under test.

use std::time::{Duration, Instant};

use librespot_core::SpotifyUri;
use tokio::sync::oneshot;

use crate::queue::{MediaSource, PriorityQueue, QueueItem};

/// How long a `Sent` arm may go unacknowledged by a `SetQueue` before it is
/// marked `Lost` (advisory only — never retried blind).
pub const ARM_ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `Active::SpotifyPending` may wait for its `Playing` before the
/// fallback runs (retry the load once while idle, else surface an error).
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a link-down arm snapshot stays eligible for restore-by-transfer.
pub const SNAPSHOT_TTL: Duration = Duration::from_secs(60);
/// Echo window for pause provenance: a `Paused` event arriving within this
/// long of a `Pause` we sent is read as our own echo, not a human pause.
pub const INFLIGHT_TTL: Duration = Duration::from_secs(2);

/// Track metadata as the core carries it — a plain struct so no transport
/// type leaks in here. The actor maps it to/from librespot lookups and
/// Discord embeds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album_art_url: Option<String>,
}

/// The turn token: who is entitled to be audible right now. Turn changes
/// only at boundaries the bot defines — media end, Spotify
/// `EndOfTrack`/`Stopped`, human skip, explicit human play. No incoming
/// event moves the turn.
#[derive(Clone, Debug)]
pub enum Active {
    None,
    /// A queue (YouTube/file) item is playing through a media runner.
    Media { item: QueueItem, paused: bool, epoch: u64 },
    /// We asked Spotify to start this exact uri (`Load`/`Next`) and are
    /// awaiting its `Playing`; `Tick(SpotifyPending)` is the escape hatch.
    SpotifyPending { uri: SpotifyUri, sent: Instant, retried: bool },
    /// The Spotify Connect baseline holds the turn.
    Spotify { track: Option<TrackMeta> },
}

/// Who paused the Spotify baseline, which decides who may auto-resume it:
/// the bot resumes only what it paused for a media item; a human pause is
/// honoured but never blocks an explicit Discord command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseOwner {
    BotForMedia,
    Human,
}

/// The one queue item pre-queued into Spotify's own queue via `AddToQueue`,
/// so librespot's auto-advance lands on it at the next boundary.
#[derive(Clone, Debug)]
pub struct Armed {
    pub item_id: u64,
    pub uri: SpotifyUri,
    pub ack: Ack,
}

/// A back-jump that has been sent to Spotify and not yet landed. It carries
/// what the arrival has to be checked against, which differs by how the jump
/// was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingJump {
    /// A context jump named one exact track. Librespot silently starts a
    /// context at track 1 when it cannot find that track, so the arrival is
    /// checked rather than assumed.
    Context(SpotifyUri),
    /// Spotify's own `Previous`, which chooses the track itself: under 3 s
    /// into a track it steps back, at or over 3 s it seeks to zero and stays
    /// put (F16). Carries where the walk lands if it moves, so the cursor is
    /// committed only once a different track actually arrives.
    Previous { cursor: i64 },
}

/// `SetQueue`-driven acknowledgement of an arm. `Lost` is advisory only:
/// librespot has no dequeue, so a blind retry after a slow ack would queue
/// the track twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    Sent(Instant),
    Confirmed,
    Lost,
}

/// Telemetry mirror of the Spotify device — an input, never the intent.
///
/// `Idle` means "ours, and holding no context", which is the one state
/// `load()` may overwrite. Two things reach it: an explicit human claim on
/// a session that never loaded anything (`ActivateDevice`, ⏯'s takeover,
/// `claim_device`), and a `Stopped` **while `device_active`**. That guard on
/// `Stopped` is the load-safety invariant: a takeover by another device
/// emits `SessionDisconnected` then `Stopped`, and without it that pair
/// would land in `Idle` and invite a `load()` over the DJ's context on a
/// device we no longer own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpDevice {
    Inactive,
    Idle,
    /// `EndOfTrack` seen; librespot's auto-advance is imminent.
    Boundary,
    Playing(SpotifyUri),
    Paused(SpotifyUri),
}

/// Mirror of the Discord voice connection. Anything that makes the bot
/// audible while this is not `Ready` emits `JoinVoice` first (see
/// `ensure_voice`). The core needs no deferred-start bookkeeping to go with
/// it: a media runner does not wait for the join, it feeds `AudioBridge`
/// straight away, and the bridge simply holds those samples until the
/// reader attaches at the end of the join. Nothing is lost either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceStatus {
    Down,
    Joining,
    Ready,
}

/// Arm state snapshotted at `LinkDown`, restorable by `Transfer` at the next
/// `LinkUp` while fresh (see `SNAPSHOT_TTL`).
#[derive(Clone, Debug)]
struct ArmSnapshot {
    armed: Armed,
    at: Instant,
}

/// Ring of `Pause` commands this core just sent, with a 2s TTL. A `Paused`
/// transport event that consumes an entry is our own echo; one that doesn't
/// is a human pausing on their device. The heuristic is accepted in both
/// directions: a real phone pause landing inside the window reads as an echo
/// and may be overridden once — the human simply pauses again.
#[derive(Debug, Default)]
struct InflightRing {
    pauses: Vec<Instant>,
}

impl InflightRing {
    fn record_pause(&mut self, now: Instant) {
        self.prune(now);
        if self.pauses.len() < 8 {
            self.pauses.push(now);
        }
    }

    /// Consume one live entry if any; `true` means "this was our echo".
    fn consume_pause(&mut self, now: Instant) -> bool {
        self.prune(now);
        if self.pauses.is_empty() {
            false
        } else {
            self.pauses.remove(0);
            true
        }
    }

    fn prune(&mut self, now: Instant) {
        self.pauses.retain(|t| now.saturating_duration_since(*t) < INFLIGHT_TTL);
    }

    fn clear(&mut self) {
        self.pauses.clear();
    }
}

/// The player's owned state. The actor holds exactly one of these; nothing
/// else reads or writes it.
pub struct PlayerState {
    /// The unified queue, owned here (radio rules: strict order across all
    /// sources; the bot never skips anything on its own).
    pub queue: PriorityQueue,
    pub active: Active,
    /// Telemetry mirror — an input, never the intent.
    pub sp: SpDevice,
    pub armed: Option<Armed>,
    pub pause_owner: Option<PauseOwner>,
    inflight: InflightRing,
    /// Whether this device is the active Connect device. Commands are
    /// silently dropped by librespot while it isn't (F2), so audibility
    /// commands gate on this. Activation is explicit — never on connect.
    pub device_active: bool,
    pub voice: VoiceStatus,
    /// Session generation; `Transport`/`LinkDown` inputs carrying another
    /// gen are stale and ignored.
    pub link_gen: u64,
    /// Whether the Spotify session link is currently up — `true` from
    /// `LinkUp` until the matching `LinkDown`. Telemetry only, surfaced by
    /// `Query`'s [`PlayerSnapshot`]; nothing here branches on it.
    pub link_up: bool,
    /// Media-runner generation; a `MediaEnded` carrying another epoch is a
    /// stale runner's report and is ignored.
    pub media_epoch: u64,
    /// Uri (as string) of the last track a turn-approved `Playing` was
    /// accepted for; suppresses duplicate cards when librespot re-emits
    /// `Playing` for the same track (seek, resume).
    pub last_heard_track: Option<String>,
    /// The DJ's shuffle/repeat settings, mirrored from Spotify so a context
    /// jump can put them back.
    pub play_options: PlayOptions,
    /// Where back-navigation has walked to in the play history. `None` means
    /// "live": the next ⏮ starts from whatever is playing. Any forward move
    /// (a skip, a play, a queued item starting) puts us back live.
    pub history_cursor: Option<i64>,
    /// The back-jump in flight, until a transport event settles it. What the
    /// arrival is checked against depends on how the jump was made — see
    /// [`PendingJump`].
    awaiting_jump: Option<PendingJump>,
    /// The ⏮ reply, held while the shell reads the history.
    pending_reply: Option<oneshot::Sender<String>>,
    /// A request Spotify started while a media item held the turn. It was
    /// popped from the queue there — it has to be, or `maybe_arm` would hand
    /// the same track to a queue librespot cannot take it back out of — and
    /// paused straight back down, so the airing history should record is the
    /// later one, after the media item. This holds who asked for it until
    /// then; without it that airing files as the DJ's own baseline.
    pending_request: Option<QueueItem>,
    /// The context the Spotify baseline is playing from, as last reported
    /// by `SetQueue`. Stamped onto history rows so a later back-jump can
    /// reopen the playlist at a track instead of replacing it with a
    /// one-track context.
    pub context_uri: Option<String>,
    armed_snapshot: Option<ArmSnapshot>,
}

impl PlayerState {
    pub fn new() -> Self {
        Self {
            queue: PriorityQueue::new(),
            active: Active::None,
            sp: SpDevice::Inactive,
            link_up: false,
            context_uri: None,
            play_options: PlayOptions::default(),
            history_cursor: None,
            awaiting_jump: None,
            pending_reply: None,
            pending_request: None,
            armed: None,
            pause_owner: None,
            inflight: InflightRing::default(),
            device_active: false,
            voice: VoiceStatus::Down,
            link_gen: 0,
            media_epoch: 0,
            last_heard_track: None,
            armed_snapshot: None,
        }
    }
}

/// Where an enqueue lands in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueuePos {
    Tail,
    Head,
    At(usize),
}

/// How a media runner ended. Both run the same boundary decision — a skip
/// emits `CancelMedia` only and the next start always comes from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaOutcome {
    Finished,
    Cancelled,
}

/// Timers the actor spawns as sleeps that come back as `Input::Tick`.
/// Handlers re-validate against the state, so a stale timer is a no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    ArmAck,
    SpotifyPending,
    SnapshotExpiry,
}

/// Where an aired track came from. Not who queued it — a request aired
/// because someone asked for it here; a baseline track aired because the
/// DJ's own Spotify context reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiredSource {
    Request,
    Baseline,
}

impl AiredSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AiredSource::Request => "request",
            AiredSource::Baseline => "baseline",
        }
    }

    /// Unknown strings read as `Baseline`: a row written by a newer version
    /// should degrade to "something played", never panic a listing.
    pub fn from_str(s: &str) -> Self {
        match s {
            "request" => AiredSource::Request,
            _ => AiredSource::Baseline,
        }
    }
}

/// Shuffle/repeat, as Spotify reports them and as a context load must
/// restore them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayOptions {
    pub shuffle: bool,
    pub repeat_context: bool,
    pub repeat_track: bool,
}

/// A track found in the play history by a ⏮, handed back to the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviousTrack {
    /// History row id — the cursor the next ⏮ walks back from.
    pub id: i64,
    pub uri: SpotifyUri,
    /// The context it aired from, when one was recorded.
    pub context_uri: Option<String>,
}

/// One track becoming audible, handed to the history store by the actor.
/// Plain strings only: the core names what aired, the store decides how to
/// keep it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiredTrack {
    pub source: AiredSource,
    /// A Spotify uri, or the URL/filename for a media item.
    pub track_ref: String,
    /// The Spotify context it aired from, when there was one.
    pub context_uri: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub queued_by: Option<String>,
    pub queued_by_id: Option<String>,
}

/// Gen-tagged Spotify telemetry, translated from librespot player events by
/// the session layer. `SetQueue.queued` carries only `provider == "queue"`
/// entries (the ones `AddToQueue` creates); `current` is unfiltered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportEvent {
    Playing { uri: SpotifyUri, meta: Option<TrackMeta> },
    Paused { uri: SpotifyUri },
    Stopped,
    EndOfTrack,
    Unavailable { uri: SpotifyUri },
    TrackChanged { uri: SpotifyUri, meta: TrackMeta },
    SetQueue {
        current: Option<SpotifyUri>,
        queued: Vec<SpotifyUri>,
        /// The context (playlist/album/station) the baseline is playing
        /// from, when Spotify names one. Only `SetQueue` carries it, and
        /// only on a context load or queue mutation — never per advance.
        context_uri: Option<String>,
    },
    /// Shuffle/repeat as Spotify reports them. Tracked so a context jump
    /// can restore them: loading a context resets these on librespot's side,
    /// which would silently turn the DJ's shuffle off.
    OptionsChanged { shuffle: bool, repeat_context: bool, repeat_track: bool },
    SessionConnected,
    SessionDisconnected,
}

/// One row of the `/queue` listing, as [`PlayerSnapshot::preview`] carries
/// it — display strings pulled from [`MediaSource`]'s own formatters, plus
/// the one fact the UI can't derive from them: whether this residency is
/// the one currently armed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEntry {
    pub item_id: u64,
    pub title: String,
    pub subtitle: String,
    pub duration: Option<String>,
    pub queued_by: String,
    pub armed: bool,
}

/// What's currently audible, as `/np` reports it. Carries no formatting —
/// rendering it is the actor/UI's job, not this pure core's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NowPlaying {
    Nothing,
    Media { title: String, subtitle: String, queued_by: String, paused: bool },
    Spotify { title: String, artist: String, paused: bool },
    /// The baseline holds (or is about to hold) the turn but no track is
    /// known yet: either `Active::SpotifyPending` awaiting its `Playing`,
    /// or a freshly reconciled `Active::Spotify { track: None }` before
    /// the next transport event fills the title in.
    SpotifyStarting,
}

/// Reply payload for [`Input::Query`] — everything `/np` and `/queue` need
/// to render, structured instead of pre-formatted so the actor (not this
/// pure core) owns the Discord-facing text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerSnapshot {
    pub now: NowPlaying,
    pub queue_len: usize,
    /// At most `QUEUE_PREVIEW` entries, in play order.
    pub preview: Vec<QueueEntry>,
    /// `queue_len - preview.len()`, i.e. how many the preview omits.
    pub more: usize,
    pub device_active: bool,
    pub link_up: bool,
}

/// How many queue entries [`PlayerSnapshot::preview`] carries.
pub const QUEUE_PREVIEW: usize = 5;

/// Everything that can happen to the player. Command inputs carry their own
/// reply channel (so `Input` is not `Clone`); `step` answers them via
/// `Effect::Reply` (`Effect::ReplySnapshot` for `Query`) — the actor never
/// formats a reply itself.
#[derive(Debug)]
pub enum Input {
    Enqueue {
        item: QueueItem,
        pos: EnqueuePos,
        /// Start the queue head right away when nothing holds the turn.
        start_if_idle: bool,
        reply: oneshot::Sender<String>,
    },
    Skip { reply: oneshot::Sender<String> },
    Stop {
        reply: oneshot::Sender<String>,
        /// `true` for a human `/stop`: the bot is in the channel and must
        /// leave it. `false` from the teardown paths, which own the voice
        /// connection's fate themselves — it is already gone (force
        /// disconnect) or removed by the caller (empty channel).
        leave_voice: bool,
    },
    TogglePause { reply: oneshot::Sender<String> },
    Previous { reply: oneshot::Sender<String> },
    /// A media runner's terminal report, epoch-tagged against stale runners.
    MediaEnded {
        epoch: u64,
        /// Informational for the core (the actor logs/announces it); kept on
        /// the input so the report is one message.
        #[allow(dead_code)]
        outcome: MediaOutcome,
    },
    Transport { gen: u64, ev: TransportEvent },
    LinkUp { gen: u64 },
    LinkDown { gen: u64 },
    /// Fast reconnect in progress — informational, never an armed-clearing
    /// event.
    LinkReconnecting { gen: u64 },
    VoiceReady,
    VoiceLost,
    /// Bare `/play`: the ▶ half of ⏯, refused while something is audible.
    Play { reply: oneshot::Sender<String> },
    /// An explicit human claim on the Connect device (`/login`): the one
    /// path besides ▶ that may activate. Auto-start and on-demand sessions
    /// never send it (F15).
    ActivateDevice,
    /// The history row the shell found for a ⏮, or `None` when there is
    /// nothing further back.
    PreviousResolved { track: Option<PreviousTrack> },
    /// Empty the queue without touching what is currently audible — the
    /// half of `/stop` that isn't "go to dead air".
    ClearQueue { reply: oneshot::Sender<String> },
    /// The queue as it was when the process last stopped, replayed at boot.
    /// Restoring never starts playback on its own — nothing is audible
    /// without a voice channel and someone in it to hear it.
    RestoreQueue { items: Vec<QueueItem> },
    Query { reply: oneshot::Sender<PlayerSnapshot> },
    Tick(TimerKind),
}

/// Spirc commands as the core names them. The actor maps these onto the
/// live session's channel; keeping a local enum means the core can name
/// commands (`ActivateDevice`, `Transfer`) before the transport grows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpircCmd {
    Pause,
    Play,
    Next,
    Previous,
    AddToQueue(SpotifyUri),
    /// Start this uri now, replacing the device's context — only ever
    /// issued while `sp == Idle && device_active`.
    Load(SpotifyUri),
    /// Claim the active-device slot. Explicit only: first `/login` or a
    /// human play/takeover — never on connect.
    ActivateDevice,
    /// `Spirc::transfer(None)`: restore context, position, pause state and
    /// the queue after a reconnect.
    Transfer,
    /// Reopen a context positioned at one of its tracks. Unlike `Load`,
    /// which replaces the context with a single track, this restores the
    /// playlist and starts inside it — the only non-destructive way to play
    /// a specific track the DJ already heard.
    LoadContext { context_uri: String, track_uri: SpotifyUri, options: PlayOptions },
    /// Give up the active-device slot. Always paired with a pause: the
    /// non-pausing form leaves librespot decoding into a bridge nobody
    /// drains, and its next `Playing` would re-take the turn from outside
    /// the voice channel.
    Disconnect,
}

/// Commands for the live songbird track handle of the active media item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackHandleCmd {
    Pause,
    Resume,
}

/// Runner-side gate on a `StartMedia`: the runner spawns immediately and
/// awaits the gate before feeding audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartGate {
    Immediate,
    /// Await the actor's notify for the matching `Transport(Paused)`, with a
    /// 500ms fallback (the actor pre-fires when `sp` is already paused).
    AfterSpotifyPauseAck,
}

/// Channel-facing messages, shaped here so the core stays Discord-free; the
/// UI task renders them.
#[derive(Clone, Debug)]
pub enum UiMsg {
    NowPlayingSpotify { uri: SpotifyUri, meta: Option<TrackMeta> },
    NowPlayingMedia { item: QueueItem },
    /// "Spotify is in use on another device — press play to take over."
    TakeoverPrompt,
    Notice(String),
}

/// DJ announce requests; the actor drops them when announcements are off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnounceKind {
    Track { title: String, artist: String },
}

/// Presence snapshot for the bot's Discord status, Spotify-side only (media
/// presence is not modelled). The actor adapts it to the presence loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresenceState {
    Idle,
    Playing { uri: SpotifyUri, meta: TrackMeta },
    Paused { uri: SpotifyUri, meta: TrackMeta },
}

/// What the actor must do after a `step`. Every effect is a synchronous
/// send or a spawn — nothing here may block the actor.
#[derive(Debug)]
pub enum Effect {
    Spirc(SpircCmd),
    StartMedia { item: QueueItem, epoch: u64, gate: StartGate },
    CancelMedia,
    ClearBridge,
    JoinVoice,
    /// A track just became audible: append it to the play history. Emitted
    /// once per airing, alongside the card that announces it.
    RecordAired(AiredTrack),
    /// Leave the voice channel. Only `/stop` asks for this — an empty
    /// channel is torn down by the Discord layer instead.
    LeaveVoice,
    /// Read the play history for the track aired before `before` (or before
    /// whatever is playing, when `None`) and feed it back as
    /// `Input::PreviousResolved`. The core never touches the database.
    ResolvePrevious { before: Option<i64> },
    /// Spotify reported a track playing without metadata: resolve it
    /// through the live session and feed the answer back as
    /// `TransportEvent::TrackChanged` (which posts the card).
    ResolveMeta(SpotifyUri),
    TrackHandle(TrackHandleCmd),
    Ui(UiMsg),
    Presence(PresenceState),
    Announce(AnnounceKind),
    SetTimer(TimerKind, Duration),
    Reply(oneshot::Sender<String>, String),
    /// `Query`'s reply: unlike every other command, its answer is
    /// structured data instead of a formatted string — the actor/UI layer
    /// (not this pure core) renders `/np` and `/queue` from it.
    ReplySnapshot(oneshot::Sender<PlayerSnapshot>, PlayerSnapshot),
}

/// Advance the player by one input. Pure and synchronous: mutates `state`,
/// returns the effects in the order the actor must run them, and reads time
/// only from `now`.
pub fn step(state: &mut PlayerState, input: Input, now: Instant) -> Vec<Effect> {
    let mut fx = Vec::new();
    match input {
        Input::Enqueue { item, pos, start_if_idle, reply: tx } => {
            let queued_title = item.source.display_title().to_string();
            let accepted = match pos {
                EnqueuePos::Tail => state.queue.push(item),
                EnqueuePos::Head => state.queue.push_front(item),
                EnqueuePos::At(idx) => state.queue.insert(idx, item),
            };
            if !accepted {
                reply(
                    &mut fx,
                    tx,
                    format!("⚠️ Queue is full ({} items) — try again once some have played.", crate::queue::MAX_QUEUE_LEN),
                );
                return fx;
            }
            let queue_len_after_push = state.queue.len();
            let mut started_title: Option<String> = None;
            if start_if_idle && matches!(state.active, Active::None) {
                match head_of(&state.queue) {
                    Head::Media => {
                        // Nothing holds the turn but the baseline may still
                        // be audible (e.g. post-stop race): pause it before
                        // the media item takes over.
                        let pausing =
                            state.device_active && matches!(state.sp, SpDevice::Playing(_));
                        if pausing {
                            fx.push(Effect::Spirc(SpircCmd::Pause));
                            state.inflight.record_pause(now);
                            state.pause_owner = Some(PauseOwner::BotForMedia);
                            fx.push(Effect::ClearBridge);
                        }
                        let head_item = state.queue.pop().expect("head checked as media");
                        started_title = Some(head_item.source.display_title().to_string());
                        let gate = if pausing {
                            StartGate::AfterSpotifyPauseAck
                        } else {
                            StartGate::Immediate
                        };
                        start_media(state, head_item, gate, &mut fx);
                    }
                    Head::Spotify(uri) => {
                        if !claim_device(state, &mut fx) {
                            // No session to play through: the handler
                            // brings one up before a Spotify enqueue, so
                            // this is a link that just died.
                            fx.push(Effect::Ui(UiMsg::TakeoverPrompt));
                        } else if matches!(state.sp, SpDevice::Idle) {
                            started_title = state
                                .queue
                                .peek()
                                .map(|i| i.source.display_title().to_string());
                            begin_load(state, uri, now, &mut fx);
                        }
                        // Paused/Boundary/Playing: arming below covers it —
                        // an enqueue never unpauses the baseline.
                    }
                    Head::Empty => {}
                }
            }
            maybe_arm(state, now, &mut fx);
            let text = match started_title {
                Some(t) if t == queued_title => "▶ Playing now".to_string(),
                Some(t) => format!("➕ Queued. **{t}** is starting first."),
                None if matches!(pos, EnqueuePos::Head) => "➕ Playing next".to_string(),
                None if !matches!(state.active, Active::None) => {
                    format!("➕ Added to queue #{queue_len_after_push}")
                }
                None => "➕ Queued. Nothing is playing right now — press ▶ or use `/play` to start."
                    .to_string(),
            };
            reply(&mut fx, tx, text);
        }

        Input::Skip { reply: tx } => {
            if matches!(state.active, Active::Media { .. }) {
                // A media skip cancels only; the next start comes from
                // `MediaEnded{epoch, Cancelled}` — feeder cancellation is
                // asynchronous, and an immediate start would let the old
                // feeder's tail bleed into the new item.
                fx.push(Effect::CancelMedia);
                fx.push(Effect::ClearBridge);
                // A human skip is an explicit advance: whatever paused the
                // baseline (a phone pause, a /stop), the next thing plays.
                // The bot owns the pause from here, so the post-media
                // boundary resumes instead of honouring it.
                if state.device_active
                    && matches!(state.sp, SpDevice::Paused(_) | SpDevice::Boundary)
                {
                    state.pause_owner = Some(PauseOwner::BotForMedia);
                }
                reply(&mut fx, tx, "⏭ Skipped.");
                return fx;
            }
            match head_of(&state.queue) {
                Head::Media => {
                    // Human skip onto a media head: pause(); next() is a
                    // silent advance (F4) — the skipped track loads paused
                    // at 0:00, so exactly one Spotify track is consumed and
                    // the resume after the media item is deterministic.
                    let pausing = state.device_active
                        && matches!(
                            state.sp,
                            SpDevice::Playing(_) | SpDevice::Paused(_) | SpDevice::Boundary
                        );
                    if pausing {
                        fx.push(Effect::Spirc(SpircCmd::Pause));
                        state.inflight.record_pause(now);
                        fx.push(Effect::Spirc(SpircCmd::Next));
                        // The skip is an explicit advance: the bot owns the
                        // pause now even if a human paused first — this is
                        // the frozen-skip fix.
                        state.pause_owner = Some(PauseOwner::BotForMedia);
                    }
                    fx.push(Effect::ClearBridge);
                    let item = state.queue.pop().expect("head checked as media");
                    let gate = if pausing {
                        StartGate::AfterSpotifyPauseAck
                    } else {
                        StartGate::Immediate
                    };
                    start_media(state, item, gate, &mut fx);
                    reply(&mut fx, tx, "⏭ Skipped.");
                }
                Head::Spotify(uri) => {
                    if !claim_device(state, &mut fx) {
                        reply(
                            &mut fx,
                            tx,
                            "⚠️ No Spotify session right now — someone needs to run `/login`.",
                        );
                        return fx;
                    }
                    if matches!(state.sp, SpDevice::Idle) {
                        begin_load(state, uri, now, &mut fx);
                        reply(&mut fx, tx, "⏭ Skipped.");
                        return fx;
                    }
                    let was_playing = matches!(state.sp, SpDevice::Playing(_));
                    let was_paused = matches!(state.sp, SpDevice::Paused(_));
                    // Queue the head behind the current context if it isn't
                    // armed yet, then advance onto it.
                    maybe_arm(state, now, &mut fx);
                    fx.push(Effect::Spirc(SpircCmd::Next));
                    if was_paused {
                        // A user-initiated advance always emits an
                        // audibility command; Next alone keeps pause state.
                        fx.push(Effect::Spirc(SpircCmd::Play));
                    }
                    if was_playing {
                        fx.push(Effect::ClearBridge);
                    }
                    state.pause_owner = None;
                    state.active = Active::SpotifyPending { uri, sent: now, retried: false };
                    fx.push(Effect::SetTimer(TimerKind::SpotifyPending, PENDING_TIMEOUT));
                    reply(&mut fx, tx, "⏭ Skipped.");
                }
                Head::Empty => {
                    if state.device_active
                        && matches!(
                            state.sp,
                            SpDevice::Playing(_) | SpDevice::Paused(_) | SpDevice::Boundary
                        )
                    {
                        let was_playing = matches!(state.sp, SpDevice::Playing(_));
                        let was_paused = matches!(state.sp, SpDevice::Paused(_));
                        fx.push(Effect::Spirc(SpircCmd::Next));
                        if was_paused {
                            fx.push(Effect::Spirc(SpircCmd::Play));
                        }
                        if was_playing {
                            fx.push(Effect::ClearBridge);
                        }
                        state.pause_owner = None;
                        reply(&mut fx, tx, "⏭ Skipped.");
                    } else {
                        reply(&mut fx, tx, "Nothing is playing right now.");
                    }
                }
            }
        }

        Input::Stop { reply: tx, leave_voice } => {
            // Stop is stop: the bot goes quiet and leaves the channel. It
            // keeps the queue (`/clear` is what empties it) and never touches
            // the Spotify session or the account — only this lifecycle.
            if matches!(state.active, Active::Media { .. }) {
                fx.push(Effect::CancelMedia);
            }
            state.armed_snapshot = None;
            // A jump still resolving is abandoned along with everything else,
            // and a request whose airing was being held will never air now.
            state.awaiting_jump = None;
            state.history_cursor = None;
            state.pending_request = None;
            // The card is gone, so the next airing is new even if it is the
            // same track: without this the duplicate-card guard also swallows
            // that airing's history row.
            state.last_heard_track = None;
            // Audibility, not the turn: Spotify reaches the bridge whenever
            // no media item holds it, `Active::None` included.
            if state.device_active {
                if matches!(state.sp, SpDevice::Playing(_)) {
                    state.inflight.record_pause(now);
                }
                fx.push(Effect::Spirc(SpircCmd::Disconnect));
                state.device_active = false;
                // `Disconnect` resets librespot's device state, queue
                // included (`became_inactive` -> `reset`, which replaces
                // `next_tracks` with an empty vec and zeroes `queue_count`;
                // connect/src/state.rs @1599145), so a surviving arm is a
                // ghost: `maybe_arm` would refuse to re-arm behind it, and
                // the next `SetQueue` would read its absence as "deleted on
                // the phone" and drop the request. Clearing it belongs to
                // this branch alone — with no `Disconnect` sent, the track
                // really is still in Spotify's queue, and dropping the arm
                // would let `maybe_arm` queue it a second time with no way
                // to take either copy back.
                state.armed = None;
                // The device is released, so the mirror must not go on
                // claiming it is playing — the same pairing `LinkDown` and
                // `SessionDisconnected` already make.
                state.sp = SpDevice::Inactive;
            }
            fx.push(Effect::ClearBridge);
            // `LeaveVoice` arms the shell's deliberate-leave guard, which is
            // consumed by Discord's echo of the removal. A teardown reacting
            // to a force disconnect has no echo coming (the bot's voice
            // state is already "no channel"), so emitting it there would
            // latch the guard and make the NEXT force disconnect read as
            // deliberate — no teardown, librespot feeding a dead call.
            if leave_voice {
                fx.push(Effect::LeaveVoice);
            }
            // The runner's `MediaEnded{Cancelled}` still lands after this;
            // `Active::None` plus a `Down` voice makes that boundary quiet,
            // and lets the next queued item ask for a fresh join.
            state.active = Active::None;
            state.voice = VoiceStatus::Down;
            state.pause_owner = None;
            fx.push(Effect::Presence(PresenceState::Idle));

            let queued = state.queue.len();
            let text = match queued {
                0 => "⏹ Stopped and left the channel.".to_string(),
                n => format!(
                    "⏹ Stopped and left the channel. {n} queued track(s) kept — use `/clear` to drop them."
                ),
            };
            reply(&mut fx, tx, &text);
        }

        Input::Play { reply: tx } => {
            // The ▶ half only: never pauses. With something audible it
            // asks for a link instead (a fat-fingered bare `/play` can't
            // cut the music); otherwise it is exactly ⏯.
            let audible = matches!(state.active, Active::Media { paused: false, .. })
                || (!matches!(state.active, Active::Media { .. })
                    && matches!(state.sp, SpDevice::Playing(_)));
            if audible {
                reply(
                    &mut fx,
                    tx,
                    "❌ Something is already playing — give `/play` a link or file, or use ⏯ to pause.",
                );
                return fx;
            }
            return step(state, Input::TogglePause { reply: tx }, now);
        }

        Input::TogglePause { reply: tx } => {
            if let Active::Media { paused, .. } = &mut state.active {
                *paused = !*paused;
                let now_paused = *paused;
                fx.push(Effect::TrackHandle(if now_paused {
                    TrackHandleCmd::Pause
                } else {
                    TrackHandleCmd::Resume
                }));
                reply(&mut fx, tx, if now_paused { "⏸ Paused" } else { "▶ Resumed" });
                return fx;
            }
            if matches!(state.sp, SpDevice::Playing(_)) {
                fx.push(Effect::Spirc(SpircCmd::Pause));
                state.inflight.record_pause(now);
                state.pause_owner = Some(PauseOwner::Human);
                reply(&mut fx, tx, "⏸ Paused");
                return fx;
            }
            // The ▶ half: nothing is audible.
            if !state.device_active {
                // Explicit human play is the takeover gesture (F15: never
                // activate on connect). Optimistically flip `device_active`;
                // gen-tagged telemetry corrects us if the claim failed.
                fx.push(Effect::Spirc(SpircCmd::ActivateDevice));
                state.device_active = true;
                if matches!(state.sp, SpDevice::Inactive) {
                    // Nothing was ever loaded on this session: activated,
                    // it's an idle device — loadable.
                    state.sp = SpDevice::Idle;
                }
                // Claiming the device is a claim on the bot: whatever the DJ
                // starts next, from Discord or from their phone, is meant to
                // come out here. Joining now (rather than when the first
                // track lands) also makes the takeover visible in the
                // channel, the way `/login` is.
                ensure_voice(state, &mut fx);
                match head_of(&state.queue) {
                    Head::Media => {
                        let item = state.queue.pop().expect("head checked as media");
                        let title = item.source.display_title().to_string();
                        start_media(state, item, StartGate::Immediate, &mut fx);
                        reply(&mut fx, tx, format!("▶ Taking over the Spotify device — starting **{title}**."));
                    }
                    Head::Spotify(uri) if matches!(state.sp, SpDevice::Idle) => {
                        let title = state
                            .queue
                            .peek()
                            .map(|i| i.source.display_title().to_string())
                            .unwrap_or_default();
                        begin_load(state, uri, now, &mut fx);
                        reply(&mut fx, tx, format!("▶ Taking over the Spotify device — starting **{title}**."));
                    }
                    _ => reply(&mut fx, tx, "▶ Taking over the Spotify device."),
                }
                return fx;
            }
            match head_of(&state.queue) {
                Head::Media => {
                    // The head airs; a pre-existing pause keeps its owner,
                    // so a phone pause still blocks the auto-resume
                    // afterwards.
                    let advancing = matches!(state.sp, SpDevice::Boundary);
                    if advancing {
                        fx.push(Effect::Spirc(SpircCmd::Pause));
                        state.inflight.record_pause(now);
                        state.pause_owner = Some(PauseOwner::BotForMedia);
                    }
                    let item = state.queue.pop().expect("head checked as media");
                    let gate = if advancing {
                        StartGate::AfterSpotifyPauseAck
                    } else {
                        StartGate::Immediate
                    };
                    start_media(state, item, gate, &mut fx);
                    reply(&mut fx, tx, "▶ Playing now");
                }
                Head::Spotify(uri) => {
                    if matches!(state.sp, SpDevice::Idle) {
                        begin_load(state, uri, now, &mut fx);
                        reply(&mut fx, tx, "▶ Playing now");
                    } else {
                        // Never Load over a paused baseline — arm the head
                        // and resume; auto-advance airs it at the boundary.
                        maybe_arm(state, now, &mut fx);
                        ensure_voice(state, &mut fx);
                        fx.push(Effect::Spirc(SpircCmd::Play));
                        state.pause_owner = None;
                        state.active = Active::Spotify { track: None };
                        reply(&mut fx, tx, "▶ Resumed");
                    }
                }
                Head::Empty => {
                    if matches!(state.sp, SpDevice::Paused(_) | SpDevice::Boundary) {
                        ensure_voice(state, &mut fx);
                        fx.push(Effect::Spirc(SpircCmd::Play));
                        state.pause_owner = None;
                        state.active = Active::Spotify { track: None };
                        reply(&mut fx, tx, "▶ Resumed");
                    } else {
                        reply(&mut fx, tx, "Nothing is playing right now.");
                    }
                }
            }
        }

        Input::Previous { reply: tx } => {
            if matches!(state.active, Active::Media { .. }) {
                reply(&mut fx, tx, "❌ Previous isn't available during queue playback.");
            } else if state.device_active
                && matches!(
                    state.sp,
                    SpDevice::Playing(_) | SpDevice::Paused(_) | SpDevice::Boundary
                )
            {
                // Our own history decides what "previous" means, not
                // Spotify's: the bot drives the account, so what the room
                // heard is the authoritative record. The answer comes back
                // as `PreviousResolved`.
                if state.pending_reply.is_some() {
                    // A ⏮ is already being resolved; answering this one
                    // separately would either drop the first caller's reply
                    // channel or walk back twice for one intent.
                    reply(&mut fx, tx, "⏮ Already going back — one moment.");
                } else {
                    fx.push(Effect::ResolvePrevious { before: state.history_cursor });
                    state.pending_reply = Some(tx);
                }
            } else {
                reply(&mut fx, tx, "Nothing is playing right now.");
            }
        }

        Input::PreviousResolved { track } => {
            let Some(tx) = state.pending_reply.take() else {
                // The reply channel is gone (a second ⏮ raced this one);
                // resolving twice is harmless, answering twice is not.
                return fx;
            };
            // The read is asynchronous, so the world may have moved on: a
            // `/stop` in the meantime released the device and left the
            // channel, and `LoadContext` would re-activate it and start
            // playing from outside the call.
            if !state.device_active {
                reply(&mut fx, tx, "❌ Nothing is playing right now.");
                return fx;
            }
            match track {
                // A track we heard, and we know the context it came from:
                // reopen the playlist positioned there, which leaves the
                // DJ's environment intact.
                Some(PreviousTrack { id, uri, context_uri: Some(context_uri) }) => {
                    state.history_cursor = Some(id);
                    state.awaiting_jump = Some(PendingJump::Context(uri.clone()));
                    fx.push(Effect::Spirc(SpircCmd::LoadContext {
                        context_uri,
                        track_uri: uri,
                        options: state.play_options,
                    }));
                    reply(&mut fx, tx, "⏮ Previous track.");
                }
                // Heard, but with no context to reopen (a one-off link, or a
                // row written before contexts were recorded). Spotify's own
                // history is the only honest fallback.
                //
                // The cursor rides along rather than being dropped: without
                // it the next ⏮ re-anchors on the newest row — the one this
                // jump is about to write — and walks forward into the track
                // it started from. It is carried on the jump rather than
                // committed here because `Previous` only steps back under
                // 3 s in (F16); the arrival decides.
                Some(PreviousTrack { id, .. }) => {
                    state.awaiting_jump = Some(PendingJump::Previous { cursor: id });
                    fx.push(Effect::Spirc(SpircCmd::Previous));
                    reply(&mut fx, tx, "⏮ Previous track.");
                }
                None => reply(
                    &mut fx,
                    tx,
                    "❌ Nothing further back — this is the earliest track I have a record of.",
                ),
            }
        }

        Input::MediaEnded { epoch, outcome: _ } => {
            // A stale runner (superseded epoch) or an already-resolved turn
            // has nothing to say. The outcome itself is informational here —
            // the actor logs and announces it; the boundary logic is the same.
            if !matches!(state.active, Active::Media { epoch: e, .. } if e == epoch) {
                return fx;
            }
            after_media_boundary(state, now, &mut fx);
        }

        Input::Transport { gen, ev } => {
            if gen != state.link_gen {
                return fx;
            }
            handle_transport(state, ev, now, &mut fx);
        }

        Input::LinkUp { gen } => {
            state.link_gen = gen;
            state.link_up = true;
            // Explicit activation only (F15): connecting never claims the
            // active device away from the DJ's phone.
            state.device_active = false;
            state.sp = SpDevice::Inactive;
            state.inflight.clear();
            // The context and the shuffle/repeat flags describe ONE account's
            // listening. A new session may be a different DJ, and carrying
            // them over would stamp the previous DJ's playlist onto the new
            // one's history rows — and open that playlist on their account
            // the first time anyone pressed ⏮.
            state.context_uri = None;
            state.play_options = PlayOptions::default();
            // Same reasoning, same account boundary: an account switch comes
            // through as a bare `LinkUp` with no `LinkDown` before it, so
            // anything the previous session left in flight has to be dropped
            // here too. A jump issued to a session that no longer exists can
            // never land, and a walk position points into another DJ's
            // listening.
            state.awaiting_jump = None;
            state.history_cursor = None;
            state.pending_request = None;
            let snapshot_ok = state.armed_snapshot.as_ref().is_some_and(|s| {
                now.saturating_duration_since(s.at) < SNAPSHOT_TTL
                    && state.queue.find_first(|i| i.item_id == s.armed.item_id).is_some()
            });
            let owe_resume = matches!(state.pause_owner, Some(PauseOwner::BotForMedia))
                && matches!(state.active, Active::None);
            if snapshot_ok || owe_resume {
                // Transfer restores context, position, pause state and the
                // queue (F12) — instead of activate, so the restored arm
                // stays queued device-side.
                fx.push(Effect::Spirc(SpircCmd::Transfer));
            }
            if snapshot_ok {
                let s = state.armed_snapshot.take().expect("snapshot checked");
                // Re-confirm the restored arm against the post-transfer
                // `SetQueue` rather than trusting the old ack.
                state.armed = Some(Armed {
                    item_id: s.armed.item_id,
                    uri: s.armed.uri,
                    ack: Ack::Sent(now),
                });
                fx.push(Effect::SetTimer(TimerKind::ArmAck, ARM_ACK_TIMEOUT));
            } else {
                state.armed_snapshot = None;
            }
            if owe_resume {
                // Media-end reconciliation: a media item that ended during
                // the outage must not leave the baseline paused forever.
                fx.push(Effect::Spirc(SpircCmd::Play));
                state.pause_owner = None;
                state.active = Active::Spotify { track: None };
            }
        }

        Input::LinkDown { gen } => {
            if gen != state.link_gen {
                return fx;
            }
            state.device_active = false;
            state.sp = SpDevice::Inactive;
            state.link_up = false;
            // Same reasoning as `SessionDisconnected`: a pending jump can no
            // longer land, so it must not outlive the link — and a request
            // whose airing was being held will never reach it.
            state.awaiting_jump = None;
            state.history_cursor = None;
            state.pending_request = None;
            // Snapshot the arm and clear it: a `Confirmed` ghost would
            // wedge arming forever, but a fresh reconnect can restore it.
            if let Some(a) = state.armed.take() {
                state.armed_snapshot = Some(ArmSnapshot { armed: a, at: now });
                fx.push(Effect::SetTimer(TimerKind::SnapshotExpiry, SNAPSHOT_TTL));
            }
            if matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. }) {
                // Session death with the baseline audible: promote a media
                // head if queued; otherwise idle plus a channel notice. The
                // media path is untouchable from here — the session
                // lifecycle cannot reach the feeder or the bridge while a
                // media item holds the turn. Spotify itself was holding the
                // turn, though, so whatever it left buffered is cleared
                // either way.
                fx.push(Effect::ClearBridge);
                if matches!(head_of(&state.queue), Head::Media) {
                    let item = state.queue.pop().expect("head checked as media");
                    start_media(state, item, StartGate::Immediate, &mut fx);
                } else {
                    state.active = Active::None;
                    fx.push(Effect::Ui(UiMsg::Notice(
                        "Spotify session lost — reconnecting. Queued items stay queued.".into(),
                    )));
                    fx.push(Effect::Presence(PresenceState::Idle));
                }
            }
        }

        Input::LinkReconnecting { gen } => {
            if gen != state.link_gen {
                return fx;
            }
            // Fast reconnects are not link-down: no armed-clearing, no turn
            // change — the session task rides it out. The session did just
            // tear down and reconnect, though, so if Spotify held the turn,
            // whatever it left buffered in the shared bridge is stale —
            // never while a media item holds the turn.
            if matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. }) {
                fx.push(Effect::ClearBridge);
            }
        }

        Input::VoiceReady => {
            state.voice = VoiceStatus::Ready;
        }

        Input::ClearQueue { reply: tx } => {
            // Empties the queue and nothing else: whatever is audible keeps
            // playing. That is the whole difference from `/stop`, which
            // clears *and* goes to dead air.
            let cleared = state.queue.len();
            let orphaned = state.armed.take().is_some();
            state.armed_snapshot = None;
            state.queue.clear();
            let text = match (cleared, orphaned) {
                (0, _) => "The queue is already empty.".to_string(),
                (n, false) => format!("🗑 Cleared {n} queued track(s)."),
                // Librespot has no dequeue, so an armed track cannot be
                // withdrawn — it may still air once when Spotify advances.
                (n, true) => format!(
                    "🗑 Cleared {n} queued track(s). (a track already handed to Spotify will still play once)"
                ),
            };
            reply(&mut fx, tx, &text);
        }

        Input::RestoreQueue { items } => {
            // Only ever at boot, onto an untouched queue: anything already
            // queued was added by someone present just now and outranks a
            // record of what the last process was holding.
            if !state.queue.is_empty() {
                return fx;
            }
            for item in items {
                // `push` stamps a fresh item_id — a restored item is a new
                // residency, not a resurrected one.
                if !state.queue.push(item) {
                    break;
                }
            }
            // Deliberately no playback: restoring is not a reason to make
            // noise. The queue airs when a human acts, or when the head is
            // reached in the ordinary way.
            maybe_arm(state, now, &mut fx);
        }

        Input::ActivateDevice => {
            if !state.device_active {
                fx.push(Effect::Spirc(SpircCmd::ActivateDevice));
                state.device_active = true;
                if matches!(state.sp, SpDevice::Inactive) {
                    // A fresh session has no transport state to report; once
                    // activated it is an idle device — the one state a
                    // Spotify head may be `Load`ed onto.
                    state.sp = SpDevice::Idle;
                }
                maybe_arm(state, now, &mut fx);
            }
        }

        Input::VoiceLost => {
            state.voice = VoiceStatus::Down;
            if matches!(state.active, Active::Media { .. }) {
                fx.push(Effect::CancelMedia);
                fx.push(Effect::ClearBridge);
                state.active = Active::None;
                // Stale-ify the runner's coming `MediaEnded` so nothing
                // tries to start the next item into a dead voice connection.
                state.media_epoch += 1;
            }
        }

        Input::Query { reply: tx } => {
            let now_playing = match &state.active {
                Active::Media { item, paused, .. } => NowPlaying::Media {
                    title: item.source.display_title().to_string(),
                    subtitle: item.source.display_subtitle(),
                    queued_by: item.queued_by.clone(),
                    paused: *paused,
                },
                Active::Spotify { track: Some(m) } => NowPlaying::Spotify {
                    title: m.title.clone(),
                    artist: m.artist.clone(),
                    paused: matches!(state.sp, SpDevice::Paused(_)),
                },
                // No cached title yet — the same "nothing to show" bucket
                // as a pending load until the next transport event lands.
                Active::Spotify { track: None } | Active::SpotifyPending { .. } => {
                    NowPlaying::SpotifyStarting
                }
                Active::None => NowPlaying::Nothing,
            };
            let items = state.queue.snapshot();
            let queue_len = items.len();
            let armed_id = state.armed.as_ref().map(|a| a.item_id);
            let preview: Vec<QueueEntry> = items
                .into_iter()
                .take(QUEUE_PREVIEW)
                .map(|item| QueueEntry {
                    item_id: item.item_id,
                    title: item.source.display_title().to_string(),
                    subtitle: item.source.display_subtitle(),
                    duration: item.source.display_duration(),
                    queued_by: item.queued_by,
                    armed: armed_id == Some(item.item_id),
                })
                .collect();
            let more = queue_len - preview.len();
            fx.push(Effect::ReplySnapshot(
                tx,
                PlayerSnapshot {
                    now: now_playing,
                    queue_len,
                    preview,
                    more,
                    device_active: state.device_active,
                    link_up: state.link_up,
                },
            ));
        }

        Input::Tick(kind) => match kind {
            TimerKind::ArmAck => {
                // Lost is advisory only — never a blind retry: a slow ack
                // plus a retry would queue the track twice, and librespot
                // has no dequeue. Re-arming happens at the next
                // armed-clearing event.
                if let Some(a) = state.armed.as_mut() {
                    if let Ack::Sent(at) = a.ack {
                        if now.saturating_duration_since(at) >= ARM_ACK_TIMEOUT {
                            a.ack = Ack::Lost;
                        }
                    }
                }
            }
            TimerKind::SpotifyPending => {
                // `SpotifyPending` always has an exit: without this, an
                // F2-dropped command parks the player forever.
                if let Active::SpotifyPending { uri, sent, retried } = &state.active {
                    if now.saturating_duration_since(*sent) >= PENDING_TIMEOUT {
                        let uri = uri.clone();
                        let retried = *retried;
                        if !state.device_active {
                            state.active = Active::None;
                            fx.push(Effect::Ui(UiMsg::TakeoverPrompt));
                        } else if matches!(state.sp, SpDevice::Idle) && !retried {
                            state.active =
                                Active::SpotifyPending { uri: uri.clone(), sent: now, retried: true };
                            fx.push(Effect::Spirc(SpircCmd::Load(uri)));
                            fx.push(Effect::SetTimer(TimerKind::SpotifyPending, PENDING_TIMEOUT));
                        } else if matches!(state.sp, SpDevice::Playing(_)) {
                            // Something started and we missed the promote —
                            // self-heal into the baseline turn.
                            state.active = Active::Spotify { track: None };
                        } else {
                            state.active = Active::None;
                            fx.push(Effect::Ui(UiMsg::Notice(
                                "Couldn't start the queued Spotify track — skipping it for now."
                                    .into(),
                            )));
                        }
                    }
                }
            }
            TimerKind::SnapshotExpiry => {
                if state
                    .armed_snapshot
                    .as_ref()
                    .is_some_and(|s| now.saturating_duration_since(s.at) >= SNAPSHOT_TTL)
                {
                    state.armed_snapshot = None;
                }
            }
        },
    }
    fx
}

/// Gen-current Spotify telemetry. Bookkeeping (queue pops, arm clears, the
/// `sp` mirror) always runs; the turn only moves at bot-defined boundaries.
fn handle_transport(state: &mut PlayerState, ev: TransportEvent, now: Instant, fx: &mut Vec<Effect>) {
    match ev {
        TransportEvent::Playing { uri, meta } => {
            state.sp = SpDevice::Playing(uri.clone());
            // Playing on this device is proof it holds the active slot —
            // this is also how a post-`Transfer` reconnect re-marks it.
            state.device_active = true;
            let uri_str = uri.to_string();
            // A context jump asked for a specific track. Librespot starts a
            // context at track 1 when it cannot find the one requested, and
            // says so only in its own log — so the arrival has to be checked
            // rather than assumed, or a bad jump silently replays a playlist
            // from the top.
            // A DIFFERENT track is the only thing that counts as movement.
            // Librespot re-emits `Playing` on a resume or a seek, and reading
            // those as movement would throw the walk away — bringing back
            // exactly the two-track bounce the cursor prevents.
            let moved = state.last_heard_track.as_deref() != Some(uri_str.as_str());
            match state.awaiting_jump.take() {
                Some(PendingJump::Context(wanted)) => {
                    if wanted != uri {
                        fx.push(Effect::Ui(UiMsg::Notice(
                            "⚠️ Spotify couldn't find that track in the playlist and started from the beginning instead."
                                .to_string(),
                        )));
                        state.history_cursor = None;
                    }
                }
                // Spotify picked the track. Over 3 s into one it seeks to
                // zero instead of stepping back (F16), which re-emits the
                // same track — no step happened, so the walk stays where it
                // was rather than claiming a move it did not make.
                Some(PendingJump::Previous { cursor }) => {
                    if moved {
                        state.history_cursor = Some(cursor);
                    }
                }
                // Nothing in flight: the playlist moved on past wherever ⏮
                // walked to, so we are live again.
                None => {
                    if moved {
                        state.history_cursor = None;
                    }
                }
            }
            // Bookkeeping first: a matching request is consumed wherever it
            // sits (a track the DJ's playlist reaches isn't also aired
            // later), and the armed marker clears only for the armed uri.
            let mut popped = state.queue.remove_first(
                |i| matches!(&i.source, MediaSource::Spotify { uri: u, .. } if u == &uri),
            );
            // Nothing left in the queue to consume, but this track was popped
            // under an earlier media turn and held: that deferred airing is
            // happening now, so take its attribution back.
            if popped.is_none()
                && state.pending_request.as_ref().is_some_and(
                    |i| matches!(&i.source, MediaSource::Spotify { uri: u, .. } if u == &uri),
                )
            {
                popped = state.pending_request.take();
            }
            // Read before the arm is cleared just below: it decides whether
            // this airing came out of Spotify's queue or out of the context.
            let from_arm = state.armed.as_ref().is_some_and(|a| a.uri == uri);
            if from_arm {
                state.armed = None;
            } else if let Active::SpotifyPending { uri: wanted, .. } = &state.active {
                // Our own advance landed somewhere other than the armed
                // track: the arm isn't at the front of Spotify's queue (a
                // dropped AddToQueue, a phone-side reorder) — a dead arm.
                // Clearing it here is what lets `maybe_arm` below issue a
                // fresh one; without this the player stayed armed-but-
                // never-acked forever and every skip played the context.
                if wanted != &uri && state.armed.is_some() {
                    state.armed = None;
                }
            }
            let same_track = state.last_heard_track.as_deref() == Some(uri_str.as_str());
            let existing = if same_track { current_meta(&state.active) } else { None };
            let meta = meta
                .or(existing)
                .or_else(|| popped.as_ref().and_then(|i| spotify_meta(&i.source)));
            if matches!(state.active, Active::Media { .. }) {
                // Not its turn: pause it back, keep the card suppressed,
                // leave the turn with the media item. It airs after the
                // queue — the resume at media end plays it from where this
                // pause lands, and *that* airing is the one history records.
                // The item has already left the queue above (it must, or
                // `maybe_arm` would hand the same track to a queue librespot
                // cannot take it back out of), so its requester is held here
                // rather than lost — otherwise the later airing files as the
                // DJ's own baseline with nobody's name on it.
                if popped.is_some() {
                    state.pending_request = popped;
                }
                fx.push(Effect::Spirc(SpircCmd::Pause));
                state.inflight.record_pause(now);
                state.pause_owner = Some(PauseOwner::BotForMedia);
                return;
            }
            // Turn-approved: the baseline holds the turn now (a pending
            // load resolving, a pending load racing something else, or a
            // plain baseline advance all land here).
            state.active = Active::Spotify { track: meta.clone() };
            state.pause_owner = None;
            // Our own librespot is decoding, so there is audio in the bridge
            // and it needs a call to drain into. Only reachable with voice
            // down after a `/stop` that left the channel while the session
            // stayed up — every involuntary voice loss tears the session
            // down, so no phone-side playback can pull the bot in here.
            ensure_voice(state, fx);
            if !same_track {
                match &meta {
                    Some(m) => {
                        fx.push(Effect::Ui(UiMsg::NowPlayingSpotify {
                            uri: uri.clone(),
                            meta: meta.clone(),
                        }));
                        fx.push(Effect::RecordAired(aired_spotify(
                            state,
                            &uri_str,
                            Some(m),
                            popped.as_ref(),
                            from_arm,
                        )));
                        fx.push(Effect::Announce(AnnounceKind::Track {
                            title: m.title.clone(),
                            artist: m.artist.clone(),
                        }));
                        state.last_heard_track = Some(uri_str);
                    }
                    None => {
                        // No card for an unknown track (it would read
                        // "Unknown track"); resolve the metadata and let the
                        // resulting `TrackChanged` post it. `last_heard_track`
                        // stays as is so that repost isn't skipped as a
                        // duplicate.
                        fx.push(Effect::ResolveMeta(uri.clone()));
                    }
                }
            }
            if let Some(m) = meta {
                fx.push(Effect::Presence(PresenceState::Playing { uri, meta: m }));
            }
            // Re-arm on every turn-approved Playing: the next arm is issued
            // a whole track ahead of the preload window, keeping
            // Spotify→Spotify gapless.
            maybe_arm(state, now, fx);
        }

        TransportEvent::Paused { uri } => {
            state.sp = SpDevice::Paused(uri.clone());
            state.device_active = true;
            if matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. }) {
                // Spotify holds the turn, so whatever it left buffered in
                // the shared bridge is stale the moment it pauses. Gated on
                // the turn, not on who caused the pause: this must never
                // fire while a media item holds the turn — a phone pause or
                // a reconnect wiping a YouTube track mid-play was the bug.
                fx.push(Effect::ClearBridge);
            }
            if !state.inflight.consume_pause(now) {
                // Nobody here asked for it: a human paused on their device.
                state.pause_owner = Some(PauseOwner::Human);
            }
            if let Active::Spotify { track: Some(m) } = &state.active {
                fx.push(Effect::Presence(PresenceState::Paused { uri, meta: m.clone() }));
            }
        }

        TransportEvent::Stopped => {
            // The `device_active` guard is load safety: a takeover by
            // another device emits SessionDisconnected (which clears the
            // flag) and then Stopped, and reading that pair as Idle would
            // invite a load() over the DJ's context on a device we no
            // longer own. See `SpDevice`.
            state.sp = if state.device_active { SpDevice::Idle } else { SpDevice::Inactive };
            state.pause_owner = None;
            if matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. }) {
                // Spotify held the turn and just stopped: clear whatever it
                // left buffered, whether or not a media item takes over next.
                fx.push(Effect::ClearBridge);
                if matches!(head_of(&state.queue), Head::Media) {
                    let item = state.queue.pop().expect("head checked as media");
                    start_media(state, item, StartGate::Immediate, fx);
                } else {
                    state.active = Active::None;
                    fx.push(Effect::Presence(PresenceState::Idle));
                }
            }
        }

        TransportEvent::EndOfTrack => {
            // Always a boundary, never Idle — auto-advance is imminent.
            state.sp = SpDevice::Boundary;
            if matches!(state.active, Active::Media { .. } | Active::SpotifyPending { .. }) {
                // Racing a media item (or a pending load): nothing to do.
                return;
            }
            match head_of(&state.queue) {
                Head::Media => {
                    // Hand the turn to the media item at the boundary. No
                    // `Pause` here: librespot ignores a pause in its
                    // EndOfTrack state (and logs an error), so the advancing
                    // next track is caught by the `Playing`-under-Media arm
                    // instead, which pauses it at ~0:00; the sink's turn
                    // gate keeps its first samples out of the bridge. The
                    // start gate still waits for that pause ack.
                    let pausing = state.device_active;
                    if pausing {
                        state.pause_owner = Some(PauseOwner::BotForMedia);
                    }
                    let item = state.queue.pop().expect("head checked as media");
                    let gate = if pausing {
                        StartGate::AfterSpotifyPauseAck
                    } else {
                        StartGate::Immediate
                    };
                    // No bridge clear: the ending track's buffered tail
                    // plays out and the media item queues in behind it.
                    start_media(state, item, gate, fx);
                }
                Head::Spotify(_) => {
                    // Arm before the boundary resolves, so librespot's
                    // auto-advance can't eat a context track first. A head
                    // that is already armed needs nothing — auto-advance
                    // lands on it by itself.
                    maybe_arm(state, now, fx);
                }
                Head::Empty => {}
            }
        }

        TransportEvent::Unavailable { uri } => {
            // Only the track we believe is audible left stale audio in the
            // bridge. Librespot also reports this for a track it was
            // preloading, which hasn't been heard at all — clearing on one
            // of those cuts the buffer out from under the track still
            // playing (a ~10s gap, since the bridge refills to capacity).
            let was_audible = match (&state.active, &state.sp) {
                (Active::SpotifyPending { uri: pending, .. }, _) => pending == &uri,
                (Active::Spotify { .. }, SpDevice::Playing(cur) | SpDevice::Paused(cur)) => {
                    cur == &uri
                }
                _ => false,
            };
            if was_audible {
                fx.push(Effect::ClearBridge);
            }
            if state.armed.as_ref().is_some_and(|a| a.uri == uri) {
                // Librespot self-skips unavailable tracks — surface it,
                // drop the request, and arm the next one.
                state.armed = None;
                let _ = state.queue.remove_first(
                    |i| matches!(&i.source, MediaSource::Spotify { uri: u, .. } if u == &uri),
                );
                fx.push(Effect::Ui(UiMsg::Notice(format!(
                    "A queued Spotify track is unavailable and was dropped ({uri})."
                ))));
                maybe_arm(state, now, fx);
            } else {
                fx.push(Effect::Ui(UiMsg::Notice(format!(
                    "Spotify reports a track as unavailable ({uri})."
                ))));
            }
        }

        TransportEvent::TrackChanged { uri, meta } => {
            // Never moves the turn. While the baseline holds it, a change
            // is shown on the card at once — a `pause(); next()` cues the
            // next track paused at 0:00 and never emits `Playing`, so
            // waiting for that left the card on the previous song.
            if matches!(state.active, Active::Media { .. }) {
                return;
            }
            if let Active::Spotify { track } = &mut state.active {
                *track = Some(meta.clone());
            }
            let uri_str = uri.to_string();
            if state.last_heard_track.as_deref() != Some(uri_str.as_str())
                && matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. })
            {
                fx.push(Effect::Ui(UiMsg::NowPlayingSpotify {
                    uri: uri.clone(),
                    meta: Some(meta.clone()),
                }));
                // The resolved-metadata path reaches the card here rather
                // than through `Playing`, so history is written here too —
                // both sites are gated on `last_heard_track`, so a track is
                // only ever recorded once per airing.
                //
                // `TrackChanged` arrives BEFORE the `Playing` that pops the
                // request — both are sent from librespot's `start_playback`,
                // in that order (playback/src/player.rs @1599145) — so the
                // queue still holds it: look it up rather than assuming this
                // is the baseline, or every request would be logged as one
                // and lose whoever asked for it.
                let queued = state
                    .queue
                    .find_first(
                        |i| matches!(&i.source, MediaSource::Spotify { uri: u, .. } if u == &uri),
                    )
                    .cloned();
                // The arm is still set here for the same reason: the
                // `Playing` that clears it has not arrived yet.
                let from_arm = state.armed.as_ref().is_some_and(|a| a.uri == uri);
                fx.push(Effect::RecordAired(aired_spotify(
                    state,
                    &uri_str,
                    Some(&meta),
                    queued.as_ref(),
                    from_arm,
                )));
                state.last_heard_track = Some(uri_str);
            }
        }

        TransportEvent::SetQueue { current, queued, context_uri } => {
            // Spotify only names the context on a load or a queue mutation,
            // so hold the last one seen rather than expecting it per track.
            if context_uri.is_some() {
                state.context_uri = context_uri;
            }
            if let Some(a) = &state.armed {
                let item_id = a.item_id;
                let ack = a.ack;
                let armed_uri = a.uri.clone();
                let present = queued.contains(&armed_uri);
                if present && !matches!(ack, Ack::Confirmed) {
                    // Sent→Confirmed, and a late ack on a Lost arm also
                    // confirms — never a second AddToQueue.
                    state.armed = Some(Armed { item_id, uri: armed_uri, ack: Ack::Confirmed });
                } else if !present
                    && matches!(ack, Ack::Confirmed)
                    && current.as_ref() != Some(&armed_uri)
                {
                    // Deleted on the phone: absent from the queue provider
                    // AND not the current track (without the current-track
                    // check this misfires exactly when auto-advance lands on
                    // the armed track). Deleting a request there is a
                    // cancel: drop the item too, then arm the next one.
                    state.armed = None;
                    let _ = state.queue.remove_first(
                        |i| matches!(&i.source, MediaSource::Spotify { uri: u, .. } if u == &armed_uri),
                    );
                    fx.push(Effect::Ui(UiMsg::Notice(
                        "A queued Spotify track was removed from the device's queue — dropped here too."
                            .into(),
                    )));
                    maybe_arm(state, now, fx);
                }
            }
        }

        TransportEvent::OptionsChanged { shuffle, repeat_context, repeat_track } => {
            // Mirror only — the DJ owns these. Kept so a context jump can
            // hand them back rather than silently resetting them.
            state.play_options = PlayOptions { shuffle, repeat_context, repeat_track };
        }

        TransportEvent::SessionConnected => {
            // Mirror only — `LinkUp` (from the session supervisor) is the
            // canonical reconnect signal and carries the new generation.
        }

        TransportEvent::SessionDisconnected => {
            state.device_active = false;
            state.sp = SpDevice::Inactive;
            // A jump can no longer land, and leaving the marker set would
            // make some later unrelated `Playing` report a mismatch for a
            // jump nobody remembers asking for. A held request's airing is
            // gone with the session too.
            state.awaiting_jump = None;
            state.history_cursor = None;
            state.pending_request = None;
            // An unacked AddToQueue was void (F2): clear it so the next
            // opportunity re-arms instead of trusting a ghost. A Confirmed
            // arm is real device-side state and survives (clearing it would
            // double-queue on restore).
            if state.armed.as_ref().is_some_and(|a| !matches!(a.ack, Ack::Confirmed)) {
                state.armed = None;
            }
        }
    }
}

/// The boundary decision after a media item finished or was cancelled:
/// decide who plays next, honouring `pause_owner`.
fn after_media_boundary(state: &mut PlayerState, now: Instant, fx: &mut Vec<Effect>) {
    match head_of(&state.queue) {
        Head::Media => {
            // Media→media: the baseline stays paused (owner unchanged); no
            // bridge clear, so the finished item's tail plays out.
            let item = state.queue.pop().expect("head checked as media");
            start_media(state, item, StartGate::Immediate, fx);
            maybe_arm(state, now, fx);
        }
        Head::Spotify(uri) => {
            if !claim_device(state, fx) {
                // Nobody to talk to (link down). Keep `pause_owner`: LinkUp
                // reconciliation pays the resume debt when the session
                // returns.
                state.active = Active::None;
                return;
            }
            if matches!(state.sp, SpDevice::Idle) {
                // No context to lose — the only situation load() is allowed.
                begin_load(state, uri, now, fx);
                return;
            }
            maybe_arm(state, now, fx);
            match state.pause_owner {
                Some(PauseOwner::BotForMedia) => {
                    // The bot paused it for the media item, so the bot
                    // resumes. If what's cued on the device isn't the head
                    // (the skip that started the media item cued the
                    // playlist's next track, F4) and the head is confirmed
                    // in Spotify's queue, advance onto it instead of
                    // resuming — the request plays before the context.
                    let cued_is_head = matches!(
                        &state.sp,
                        SpDevice::Paused(u) | SpDevice::Playing(u) if *u == uri
                    );
                    let head_confirmed = state
                        .armed
                        .as_ref()
                        .is_some_and(|a| a.uri == uri && matches!(a.ack, Ack::Confirmed));
                    state.pause_owner = None;
                    if !cued_is_head && head_confirmed {
                        fx.push(Effect::Spirc(SpircCmd::Next));
                        fx.push(Effect::Spirc(SpircCmd::Play));
                        state.active =
                            Active::SpotifyPending { uri, sent: now, retried: false };
                        fx.push(Effect::SetTimer(TimerKind::SpotifyPending, PENDING_TIMEOUT));
                    } else {
                        fx.push(Effect::Spirc(SpircCmd::Play));
                        state.active = Active::Spotify { track: None };
                    }
                }
                Some(PauseOwner::Human) => {
                    // Honoured: the baseline stays paused; the armed head
                    // airs whenever a human resumes.
                    state.active = Active::None;
                }
                None => {
                    state.active = if matches!(state.sp, SpDevice::Playing(_)) {
                        Active::Spotify { track: None }
                    } else {
                        Active::None
                    };
                }
            }
        }
        Head::Empty => {
            if matches!(state.pause_owner, Some(PauseOwner::BotForMedia))
                && state.device_active
                && matches!(state.sp, SpDevice::Paused(_) | SpDevice::Boundary)
            {
                fx.push(Effect::Spirc(SpircCmd::Play));
                state.pause_owner = None;
                state.active = Active::Spotify { track: None };
            } else if matches!(state.sp, SpDevice::Playing(_)) {
                state.active = Active::Spotify { track: None };
            } else {
                // A human pause stays honoured, and so does no owner at all;
                // a BotForMedia debt while the device is unreachable stays
                // recorded for LinkUp.
                state.active = Active::None;
                fx.push(Effect::Presence(PresenceState::Idle));
            }
        }
    }
}

/// A Spotify request reaching its turn claims the Connect device: queuing
/// the track *was* the request to hear it here. Only a session coming up
/// on its own (boot, on-demand) never claims (F15). Returns `false` when
/// there is no session to claim through.
fn claim_device(state: &mut PlayerState, fx: &mut Vec<Effect>) -> bool {
    if state.device_active {
        return true;
    }
    if !state.link_up {
        return false;
    }
    fx.push(Effect::Spirc(SpircCmd::ActivateDevice));
    state.device_active = true;
    if matches!(state.sp, SpDevice::Inactive) {
        state.sp = SpDevice::Idle;
    }
    true
}

/// Arm exactly one track: when nothing is armed, this device is active, the
/// baseline has a context (`Playing`/`Paused`/`Boundary`) and the queue
/// holds a Spotify item anywhere, queue the first such item behind the
/// current context. Arming before the boundary is what stops librespot's
/// auto-advance from eating a context track. No-op while a load is pending
/// (arming the pending uri would double it).
fn maybe_arm(state: &mut PlayerState, now: Instant, fx: &mut Vec<Effect>) {
    if state.armed.is_some() || !state.device_active {
        return;
    }
    if matches!(state.active, Active::SpotifyPending { .. }) {
        return;
    }
    if !matches!(
        state.sp,
        SpDevice::Playing(_) | SpDevice::Paused(_) | SpDevice::Boundary
    ) {
        return;
    }
    if let Some((item_id, uri)) = first_spotify(&state.queue) {
        state.armed = Some(Armed { item_id, uri: uri.clone(), ack: Ack::Sent(now) });
        fx.push(Effect::Spirc(SpircCmd::AddToQueue(uri)));
        fx.push(Effect::SetTimer(TimerKind::ArmAck, ARM_ACK_TIMEOUT));
    }
}

/// Take the turn with a `Load`: only ever called while `sp == Idle &&
/// device_active` (a load destroys the DJ's context otherwise). Sets up the
/// pending state and its escape-hatch timer.
fn begin_load(state: &mut PlayerState, uri: SpotifyUri, now: Instant, fx: &mut Vec<Effect>) {
    ensure_voice(state, fx);
    state.pause_owner = None;
    // A `Load` replaces the device's context, so the name we are holding is
    // stale the moment it is sent. Spotify reports a bare track's context as
    // an empty string, which arrives as `None` and so never overwrites — left
    // standing, the old playlist gets stamped onto this track's history row
    // and a later ⏮ reopens a playlist the track was never in.
    state.context_uri = None;
    state.active = Active::SpotifyPending { uri: uri.clone(), sent: now, retried: false };
    fx.push(Effect::Spirc(SpircCmd::Load(uri)));
    fx.push(Effect::SetTimer(TimerKind::SpotifyPending, PENDING_TIMEOUT));
}

/// Bring the voice connection up for audio that is about to exist. Idempotent
/// and safe to call on a path that turns out to make no sound — a join into a
/// call the bot is already in is a no-op in the shell.
///
/// Spotify audio needs this exactly as much as a media item does: it reaches
/// Discord through the same bridge, and the bridge is only drained by a live
/// call. So every route to audio calls this, not just the media ones — a
/// Spotify path that skips it decodes into a bridge nothing reads, and the
/// card reads "playing" over a silent channel.
fn ensure_voice(state: &mut PlayerState, fx: &mut Vec<Effect>) {
    if matches!(state.voice, VoiceStatus::Ready) {
        return;
    }
    if matches!(state.voice, VoiceStatus::Down) {
        fx.push(Effect::JoinVoice);
    }
    state.voice = VoiceStatus::Joining;
}

/// A history row for a Spotify track that just became audible. `popped` is
/// the queue item this airing consumed, when there was one — its presence is
/// what makes this a request rather than the DJ's own context reaching a
/// track, and it carries who asked for it.
///
/// `from_arm` says the track came off the arm, i.e. out of Spotify's own
/// queue rather than out of the context. Such a track is not in the playlist
/// that happens to be loaded, so it is recorded with no context: stamping one
/// makes a later ⏮ ask for a track the context does not contain, which
/// librespot answers by restarting that context from the top. It is a
/// narrower test than `popped.is_some()`, because the pop also fires when the
/// DJ's own context reaches a track that was queued — and that airing really
/// is inside the context.
fn aired_spotify(
    state: &PlayerState,
    uri_str: &str,
    meta: Option<&TrackMeta>,
    popped: Option<&QueueItem>,
    from_arm: bool,
) -> AiredTrack {
    AiredTrack {
        source: match popped {
            Some(_) => AiredSource::Request,
            None => AiredSource::Baseline,
        },
        track_ref: uri_str.to_string(),
        context_uri: if from_arm { None } else { state.context_uri.clone() },
        title: meta.map(|m| m.title.clone()),
        artist: meta.map(|m| m.artist.clone()),
        queued_by: popped.map(|i| i.queued_by.clone()),
        queued_by_id: popped.map(|i| i.queued_by_id.to_string()),
    }
}

/// Hand the turn to a media item: bump the epoch, make sure voice is coming
/// up, and emit the gated start plus its card.
fn start_media(state: &mut PlayerState, item: QueueItem, gate: StartGate, fx: &mut Vec<Effect>) {
    state.media_epoch += 1;
    // The media card replaces whatever was up: the next Spotify `Playing`
    // is new to the card even if it's the same track as before.
    state.last_heard_track = None;
    ensure_voice(state, fx);
    fx.push(Effect::StartMedia { item: item.clone(), epoch: state.media_epoch, gate });
    fx.push(Effect::Ui(UiMsg::NowPlayingMedia { item: item.clone() }));
    fx.push(Effect::RecordAired(AiredTrack {
        // Always a request: nothing but a queued item ever starts here.
        source: AiredSource::Request,
        track_ref: match &item.source {
            MediaSource::YouTube { url, .. } => url.clone(),
            MediaSource::File { attachment_url, .. } => attachment_url.clone(),
            MediaSource::Spotify { uri, .. } => uri.to_string(),
        },
        // A media item has no Spotify context to reopen.
        context_uri: None,
        title: Some(item.source.display_title().to_string()),
        artist: Some(item.source.display_subtitle()),
        queued_by: Some(item.queued_by.clone()),
        queued_by_id: Some(item.queued_by_id.to_string()),
    }));
    state.active = Active::Media { item, paused: false, epoch: state.media_epoch };
}

/// What sits at the head of the queue, with a Spotify head's uri cloned out
/// so callers can mutate the queue afterwards.
enum Head {
    Empty,
    Media,
    Spotify(SpotifyUri),
}

fn head_of(queue: &PriorityQueue) -> Head {
    match queue.peek() {
        None => Head::Empty,
        Some(item) => match &item.source {
            MediaSource::Spotify { uri, .. } => Head::Spotify(uri.clone()),
            _ => Head::Media,
        },
    }
}

/// The first Spotify item anywhere in the queue (arming looks past media
/// heads on purpose).
fn first_spotify(queue: &PriorityQueue) -> Option<(u64, SpotifyUri)> {
    let item = queue.find_first(|i| matches!(i.source, MediaSource::Spotify { .. }))?;
    match &item.source {
        MediaSource::Spotify { uri, .. } => Some((item.item_id, uri.clone())),
        _ => None,
    }
}

fn spotify_meta(source: &MediaSource) -> Option<TrackMeta> {
    match source {
        MediaSource::Spotify { title, artist, album_art_url, .. } => Some(TrackMeta {
            title: title.clone(),
            artist: artist.clone(),
            album_art_url: album_art_url.clone(),
        }),
        _ => None,
    }
}

fn current_meta(active: &Active) -> Option<TrackMeta> {
    match active {
        Active::Spotify { track } => track.clone(),
        _ => None,
    }
}

/// House style for every string `step` hands back to a human — the sole
/// place the actor's voice lives, since the actor never formats a reply
/// itself. A prefix glyph carries exactly one meaning and nothing else
/// ever leads a reply: `✅` a thing was done, `➕` added to the queue,
/// `▶`/`⏸`/`⏭`/`⏮`/`⏹` a transport action, `⚠️` (always with the variation
/// selector, never bare `⚠`) something didn't work, `❌` the request was
/// invalid or refused — nothing else prefixes a reply. Commands are
/// backticked (`/login`, `/play`); track and user names are **bold**.
/// Every state gets exactly one phrasing: a missing session always reads
/// ``No Spotify session — run `/login` to connect.``, nothing audible
/// always reads `Nothing is playing right now.` — and every failure names
/// the next action instead of just stating that it failed. Nothing here
/// ever echoes a raw yt-dlp/reqwest/parser error; swap in a fixed
/// sentence and let the caller's `tracing::warn!` keep the detail.
fn reply(fx: &mut Vec<Effect>, tx: oneshot::Sender<String>, text: impl Into<String>) {
    fx.push(Effect::Reply(tx, text.into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- fixtures -------------------------------------------------------

    fn uri(n: u64) -> SpotifyUri {
        SpotifyUri::from_uri(&format!("spotify:track:{n:022}")).unwrap()
    }

    fn meta(title: &str) -> TrackMeta {
        TrackMeta { title: title.into(), artist: "artist".into(), album_art_url: None }
    }

    fn media_item(title: &str) -> QueueItem {
        QueueItem::new(
            MediaSource::YouTube {
                url: "u".into(),
                video_id: "v".into(),
                title: title.into(),
                channel: "c".into(),
                thumbnail_url: None,
                duration_secs: 60,
            },
            "dj".into(),
            1,
        )
    }

    fn spotify_item(n: u64, title: &str) -> QueueItem {
        QueueItem::new(
            MediaSource::Spotify {
                uri: uri(n),
                title: title.into(),
                artist: "artist".into(),
                album_art_url: None,
            },
            "dj".into(),
            1,
        )
    }

    /// A `PlayerState` plus a fake clock, driven through `step` only.
    /// Situation constructors set the state up; assertions stay on the
    /// returned effects wherever possible.
    struct Sim {
        s: PlayerState,
        now: Instant,
    }

    impl Sim {
        fn new() -> Self {
            let mut s = PlayerState::new();
            s.voice = VoiceStatus::Ready;
            s.link_gen = 1;
            Self { s, now: Instant::now() }
        }

        /// Active Spotify baseline playing track 9, device active.
        fn baseline_playing() -> Self {
            let mut sim = Self::new();
            sim.s.device_active = true;
            sim.s.sp = SpDevice::Playing(uri(9));
            sim.s.active = Active::Spotify { track: None };
            sim.s.last_heard_track = Some(uri(9).to_string());
            sim
        }

        /// Baseline paused on track 9 with the given pause owner.
        fn baseline_paused(owner: Option<PauseOwner>) -> Self {
            let mut sim = Self::baseline_playing();
            sim.s.sp = SpDevice::Paused(uri(9));
            sim.s.pause_owner = owner;
            sim
        }

        /// Device active but nothing loaded (`sp == Idle`), no turn holder.
        fn idle_device() -> Self {
            let mut sim = Self::new();
            sim.s.device_active = true;
            sim.s.sp = SpDevice::Idle;
            sim
        }

        /// A media item holds the turn over a bot-paused baseline (the
        /// normal mid-drain situation).
        fn media_over_paused_baseline() -> Self {
            let mut sim = Self::baseline_paused(Some(PauseOwner::BotForMedia));
            sim.s.queue.push(media_item("active-item"));
            let item = sim.s.queue.pop().unwrap();
            sim.s.media_epoch = 1;
            sim.s.active =
                Active::Media { item, paused: false, epoch: 1 };
            sim
        }

        fn step(&mut self, input: Input) -> Vec<Effect> {
            step(&mut self.s, input, self.now)
        }

        fn advance(&mut self, d: Duration) {
            self.now += d;
        }

        fn transport(&mut self, ev: TransportEvent) -> Vec<Effect> {
            let gen = self.s.link_gen;
            self.step(Input::Transport { gen, ev })
        }

        fn enqueue(&mut self, item: QueueItem) -> Vec<Effect> {
            let (tx, _rx) = oneshot::channel();
            self.step(Input::Enqueue { item, pos: EnqueuePos::Tail, start_if_idle: false, reply: tx })
        }

        fn enqueue_start(&mut self, item: QueueItem) -> Vec<Effect> {
            let (tx, _rx) = oneshot::channel();
            self.step(Input::Enqueue { item, pos: EnqueuePos::Tail, start_if_idle: true, reply: tx })
        }

        fn skip(&mut self) -> Vec<Effect> {
            let (tx, _rx) = oneshot::channel();
            self.step(Input::Skip { reply: tx })
        }

        fn stop(&mut self) -> Vec<Effect> {
            let (tx, _rx) = oneshot::channel();
            self.step(Input::Stop { reply: tx, leave_voice: true })
        }

        /// The teardown shape: `VoiceLost` then a stop that owns no leave.
        fn teardown(&mut self) -> Vec<Effect> {
            let mut fx = self.step(Input::VoiceLost);
            let (tx, _rx) = oneshot::channel();
            fx.extend(self.step(Input::Stop { reply: tx, leave_voice: false }));
            fx
        }

        fn toggle(&mut self) -> Vec<Effect> {
            let (tx, _rx) = oneshot::channel();
            self.step(Input::TogglePause { reply: tx })
        }

        fn previous(&mut self) -> Vec<Effect> {
            let (tx, _rx) = oneshot::channel();
            self.step(Input::Previous { reply: tx })
        }

        fn media_ended(&mut self, outcome: MediaOutcome) -> Vec<Effect> {
            let epoch = self.s.media_epoch;
            self.step(Input::MediaEnded { epoch, outcome })
        }

        /// Push a Spotify item and return its stamped id.
        fn push_spotify(&mut self, n: u64, title: &str) -> u64 {
            assert!(self.s.queue.push(spotify_item(n, title)));
            self.s.queue.snapshot().last().unwrap().item_id
        }

        fn arm(&mut self, n: u64, item_id: u64, ack: Ack) {
            self.s.armed = Some(Armed { item_id, uri: uri(n), ack });
        }
    }

    // --- effect assertions ----------------------------------------------

    fn spircs(fx: &[Effect]) -> Vec<SpircCmd> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Spirc(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    fn add_to_queue_count(fx: &[Effect]) -> usize {
        spircs(fx).iter().filter(|c| matches!(c, SpircCmd::AddToQueue(_))).count()
    }

    fn has_start_media(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::StartMedia { .. }))
    }

    fn start_gate(fx: &[Effect]) -> Option<StartGate> {
        fx.iter().find_map(|e| match e {
            Effect::StartMedia { gate, .. } => Some(*gate),
            _ => None,
        })
    }

    fn has_cancel(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::CancelMedia))
    }

    fn has_leave(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::LeaveVoice))
    }

    fn has_join(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::JoinVoice))
    }

    fn has_clear(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::ClearBridge))
    }

    fn has_spotify_card(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::NowPlayingSpotify { .. })))
    }

    fn has_presence(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Presence(_)))
    }

    fn has_takeover_prompt(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::TakeoverPrompt)))
    }

    fn has_notice(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::Notice(_))))
    }

    fn timer_set(fx: &[Effect], kind: TimerKind) -> bool {
        fx.iter().any(|e| matches!(e, Effect::SetTimer(k, _) if *k == kind))
    }

    fn reply_text(fx: &[Effect]) -> String {
        fx.iter()
            .find_map(|e| match e {
                Effect::Reply(_, t) => Some(t.clone()),
                _ => None,
            })
            .expect("a command always replies")
    }

    fn reply_snapshot(fx: &[Effect]) -> PlayerSnapshot {
        fx.iter()
            .find_map(|e| match e {
                Effect::ReplySnapshot(_, s) => Some(s.clone()),
                _ => None,
            })
            .expect("a query always replies")
    }

    /// Effects minus the reply — for "this input does nothing" assertions
    /// on command inputs, which always answer their caller.
    fn without_reply(fx: Vec<Effect>) -> Vec<Effect> {
        fx.into_iter().filter(|e| !matches!(e, Effect::Reply(..))).collect()
    }

    // --- arming at enqueue (old enqueue_* table rows) ---------------------

    #[test]
    fn enqueue_arms_first_spotify_anywhere_while_playing() {
        // Arming looks past the media head for the first Spotify item.
        let mut sim = Sim::baseline_playing();
        sim.s.queue.push(media_item("m1"));
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
        assert!(timer_set(&fx, TimerKind::ArmAck));
        assert!(matches!(sim.s.armed, Some(Armed { ack: Ack::Sent(_), .. })));
    }

    #[test]
    fn enqueue_never_double_arms() {
        // One armed track at a time: a second enqueue changes nothing
        // device-side.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.enqueue(spotify_item(2, "s2"));
        assert_eq!(add_to_queue_count(&fx), 0, "one armed track at a time");
    }

    #[test]
    fn enqueue_during_media_arms_but_never_disturbs_the_media() {
        // Arming while the baseline sits bot-paused under a media item is
        // deliberate: it sets up the paused-at-0:00 handoff, and it is the
        // frozen-skip fix. Arming is not disturbing — an enqueue during
        // media still never starts or silences anything.
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
        assert!(!has_start_media(&fx) && !has_cancel(&fx) && !has_clear(&fx));
        assert!(matches!(sim.s.active, Active::Media { .. }));
    }

    #[test]
    fn enqueue_does_not_arm_while_spotify_is_idle() {
        // An idle device holds no context, so there is nothing to queue
        // behind — compare the paused case below, which does arm.
        let mut sim = Sim::idle_device();
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert!(spircs(&fx).is_empty());
        assert!(sim.s.armed.is_none());
        assert_eq!(
            reply_text(&fx),
            "➕ Queued. Nothing is playing right now — press ▶ or use `/play` to start."
        );
    }

    #[test]
    fn enqueue_arms_while_the_baseline_is_paused() {
        // A paused context is still a context. Arming into it is
        // deliberate: it is what makes a later skip or resume air the
        // request rather than freeze on it.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
    }

    #[test]
    fn enqueue_next_while_playing_reports_playing_next() {
        // `next: true` from `/play` maps to `EnqueuePos::Head`; something
        // already holds the turn, so the item queues at the front without
        // starting.
        let mut sim = Sim::baseline_playing();
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Enqueue {
            item: spotify_item(1, "s1"),
            pos: EnqueuePos::Head,
            start_if_idle: true,
            reply: tx,
        });
        assert_eq!(reply_text(&fx), "➕ Playing next");
    }

    #[test]
    fn enqueue_start_when_an_earlier_queued_item_wins_the_head() {
        // A `/queue`'d item sits unstarted while idle (queuing never
        // starts); a later `/play` finds it still at the head and starts
        // that instead — the new item queues in behind it.
        let mut sim = Sim::idle_device();
        sim.enqueue(media_item("a"));
        let fx = sim.enqueue_start(media_item("b"));
        assert_eq!(reply_text(&fx), "➕ Queued. **a** is starting first.");
        assert!(matches!(sim.s.active, Active::Media { .. }));
        assert_eq!(sim.s.queue.len(), 1, "b is still waiting");
    }

    // --- EndOfTrack boundaries (old track_end_* rows) ---------------------

    #[test]
    fn end_of_track_with_armed_head_is_hands_off() {
        // Auto-advance lands on the armed track by itself.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(spircs(&fx).is_empty());
        assert_eq!(sim.s.sp, SpDevice::Boundary);
    }

    #[test]
    fn end_of_track_arms_an_unarmed_spotify_head() {
        let mut sim = Sim::baseline_playing();
        sim.push_spotify(1, "s1");
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
    }

    #[test]
    fn end_of_track_with_media_head_starts_it_behind_the_pause_ack() {
        // No Pause at the boundary —
        // librespot ignores one in its EndOfTrack state; the advancing
        // next track is caught by the Playing-under-Media arm, and the
        // media item starts behind that pause's ack.
        let mut sim = Sim::baseline_playing();
        sim.s.queue.push(media_item("m"));
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(spircs(&fx).is_empty(), "no pause at a boundary");
        assert_eq!(start_gate(&fx), Some(StartGate::AfterSpotifyPauseAck));
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForMedia));
        assert!(matches!(sim.s.active, Active::Media { .. }));
        // The auto-advance lands: it's not its turn, so it is paused back.
        let fx = sim.transport(TransportEvent::Playing { uri: uri(10), meta: None });
        assert_eq!(spircs(&fx), vec![SpircCmd::Pause]);
        assert!(matches!(sim.s.active, Active::Media { .. }));
    }

    #[test]
    fn dead_arm_clears_and_rearms_when_our_next_lands_elsewhere() {
        // The live stuck-queue bug: an arm that never acks (dropped
        // AddToQueue) stayed `Some` forever, so nothing re-armed and every
        // skip played the context. Our own advance landing on a non-armed
        // track proves the arm is dead.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Lost);
        let fx = sim.skip();
        assert_eq!(add_to_queue_count(&fx), 0, "still armed: no second AddToQueue");
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
        // Spotify advanced onto a context track instead of s1.
        let fx = sim.transport(TransportEvent::Playing { uri: uri(42), meta: None });
        assert_eq!(sim.s.queue.len(), 1, "s1 is still a request");
        assert!(
            matches!(&sim.s.armed, Some(Armed { uri: u, ack: Ack::Sent(_), .. }) if *u == uri(1)),
            "a fresh arm was issued: {:?}",
            sim.s.armed
        );
        assert_eq!(add_to_queue_count(&fx), 1);
    }

    #[test]
    fn activate_device_input_activates_once_and_arms() {
        let mut sim = Sim::new();
        sim.s.sp = SpDevice::Paused(uri(9));
        sim.push_spotify(1, "s1");
        let fx = sim.step(Input::ActivateDevice);
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice, SpircCmd::AddToQueue(uri(1))]);
        assert!(sim.s.device_active);
        let fx = sim.step(Input::ActivateDevice);
        assert!(fx.is_empty(), "already active: nothing to do");
    }

    #[test]
    fn activating_a_fresh_session_makes_it_idle_so_a_spotify_head_loads() {
        // Live H: logout → login during a media item, then skip onto a
        // queued Spotify track did nothing — `sp` stayed Inactive after
        // ActivateDevice, so the post-media boundary had no Load path.
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.sp = SpDevice::Inactive;
        sim.s.device_active = false;
        sim.step(Input::ActivateDevice);
        assert_eq!(sim.s.sp, SpDevice::Idle);
        sim.push_spotify(1, "s1");
        sim.skip();
        let fx = sim.media_ended(MediaOutcome::Cancelled);
        assert_eq!(spircs(&fx), vec![SpircCmd::Load(uri(1))]);
    }

    #[test]
    fn track_changed_under_baseline_repaints_the_card_once() {
        // pause(); next() cues the next track without a Playing: the card
        // follows TrackChanged, and the later Playing doesn't repost.
        let mut sim = Sim::baseline_playing();
        let meta = TrackMeta { title: "t".into(), artist: "a".into(), album_art_url: None };
        let fx = sim.transport(TransportEvent::TrackChanged { uri: uri(10), meta: meta.clone() });
        assert!(fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::NowPlayingSpotify { .. }))));
        let fx = sim.transport(TransportEvent::Playing { uri: uri(10), meta: Some(meta) });
        assert!(!fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::NowPlayingSpotify { .. }))));
    }

    #[test]
    fn track_changed_under_media_is_ignored() {
        let mut sim = Sim::media_over_paused_baseline();
        let meta = TrackMeta { title: "t".into(), artist: "a".into(), album_art_url: None };
        let fx = sim.transport(TransportEvent::TrackChanged { uri: uri(10), meta });
        assert!(fx.is_empty());
    }

    #[test]
    fn end_of_track_during_media_changes_nothing() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(fx.is_empty());
        assert!(matches!(sim.s.active, Active::Media { .. }));
    }

    #[test]
    fn end_of_track_with_empty_queue_lets_the_baseline_roll() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(fx.is_empty());
    }

    // --- media-end boundaries (old media_end_* rows) ----------------------

    #[test]
    fn media_end_resumes_an_armed_spotify_head() {
        // The cued track
        // (9) isn't the confirmed head (1): advance onto the head, then
        // play — the request beats the context track the skip cued.
        let mut sim = Sim::media_over_paused_baseline();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Next, SpircCmd::Play]);
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
        assert_eq!(sim.s.pause_owner, None);
    }

    #[test]
    fn media_end_arms_and_resumes_an_unarmed_spotify_head() {
        // Queue it behind the context, then resume.
        let mut sim = Sim::media_over_paused_baseline();
        sim.push_spotify(1, "s1");
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1)), SpircCmd::Play]);
    }

    #[test]
    fn media_end_loads_a_spotify_head_while_idle() {
        // No context to lose, so load() is allowed.
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.sp = SpDevice::Idle;
        sim.s.pause_owner = None;
        sim.push_spotify(1, "s1");
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Load(uri(1))]);
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
        assert!(timer_set(&fx, TimerKind::SpotifyPending));
    }

    #[test]
    fn media_end_with_empty_queue_resumes_the_bot_paused_baseline() {
        // Nothing queued, so the boundary is quiet: whether the baseline
        // comes back is the pause owner's decision, not the boundary's.
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
        assert_eq!(sim.s.pause_owner, None);
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
    }

    #[test]
    fn media_end_with_empty_queue_and_idle_baseline_goes_quiet() {
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.sp = SpDevice::Idle;
        sim.s.pause_owner = None;
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert!(spircs(&fx).is_empty());
        assert!(matches!(sim.s.active, Active::None));
    }

    // --- human skip (old skip_* rows) -------------------------------------

    #[test]
    fn skip_with_armed_head_sends_next() {
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.skip();
        assert_eq!(spircs(&fx), vec![SpircCmd::Next]);
        assert!(
            matches!(&sim.s.active, Active::SpotifyPending { uri: u, .. } if *u == uri(1)),
            "the skip expects exactly that uri to start"
        );
    }

    #[test]
    fn skip_with_unarmed_spotify_head_arms_then_nexts() {
        let mut sim = Sim::baseline_playing();
        sim.push_spotify(1, "s1");
        let fx = sim.skip();
        let cmds = spircs(&fx);
        assert_eq!(cmds[0], SpircCmd::AddToQueue(uri(1)), "queue behind current first");
        assert!(cmds.contains(&SpircCmd::Next));
    }

    #[test]
    fn skip_onto_a_media_head_pauses_advances_and_starts_it() {
        // pause() then next() is librespot's silent advance: it loads the
        // skipped track
        // paused at 0:00 — no blip, exactly one track consumed.
        let mut sim = Sim::baseline_playing();
        sim.s.queue.push(media_item("m"));
        let fx = sim.skip();
        assert_eq!(spircs(&fx), vec![SpircCmd::Pause, SpircCmd::Next]);
        assert!(has_clear(&fx));
        assert_eq!(start_gate(&fx), Some(StartGate::AfterSpotifyPauseAck));
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForMedia));
    }

    #[test]
    fn skip_with_empty_queue_nexts_the_baseline() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.skip();
        assert_eq!(spircs(&fx), vec![SpircCmd::Next]);
    }

    // --- the play button (old play_button_* rows) -------------------------

    #[test]
    fn play_when_paused_with_armed_head_resumes() {
        let mut sim = Sim::baseline_paused(None);
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
    }

    #[test]
    fn play_when_idle_loads_the_spotify_head() {
        let mut sim = Sim::idle_device();
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::Load(uri(1))]);
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
        assert!(timer_set(&fx, TimerKind::SpotifyPending));
    }

    #[test]
    fn play_with_a_paused_baseline_arms_and_resumes_never_loads() {
        // A paused context is still the DJ's: queue behind it and resume,
        // never Load.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1)), SpircCmd::Play]);
    }

    #[test]
    fn play_with_a_media_head_starts_it_and_leaves_the_baseline_alone() {
        // The baseline stays paused and keeps its owner, so a human pause
        // still blocks the auto-resume once the media item ends.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        sim.s.queue.push(media_item("m"));
        let fx = sim.toggle();
        assert!(spircs(&fx).is_empty());
        assert!(has_start_media(&fx));
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::Human));
    }

    #[test]
    fn play_with_empty_queue_resumes_the_baseline() {
        // An explicit Discord command overrides even a human phone pause.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
        assert_eq!(sim.s.pause_owner, None);
    }

    // --- regressions, each named after its bug ----------------------------

    #[test]
    fn frozen_skip_phone_pause_then_skip_still_plays() {
        // The live repro: phone pause → ⏭ onto media → ⏭ again → the next
        // queued Spotify track must PLAY, not sit armed and silent.
        let mut sim = Sim::baseline_playing();

        // Phone pause — no inflight entry, so it's the human's.
        sim.transport(TransportEvent::Paused { uri: uri(9) });
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::Human));

        // Queue a media item, then the Spotify track: arming while paused
        // is what stages the fix.
        sim.enqueue(media_item("sc"));
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(add_to_queue_count(&fx), 1);

        // ⏭ onto the media head: pause(); next() — and the *skip* hands the
        // pause to the bot, superseding the human pause.
        let fx = sim.skip();
        assert_eq!(spircs(&fx), vec![SpircCmd::Pause, SpircCmd::Next]);
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForMedia));

        // Librespot echoes the pause on the advanced-to armed track.
        sim.transport(TransportEvent::Paused { uri: uri(1) });
        assert_eq!(
            sim.s.pause_owner,
            Some(PauseOwner::BotForMedia),
            "our echo must not read as a human pause"
        );

        // ⏭ again cancels the media item...
        let fx = sim.skip();
        assert!(has_cancel(&fx));

        // ...and the boundary decision must make sound — the old bot's
        // `resume_spotify_after_drain` said "wasn't playing" and froze here.
        let fx = sim.media_ended(MediaOutcome::Cancelled);
        assert_eq!(spircs(&fx), vec![SpircCmd::Play], "the armed track plays");
    }

    #[test]
    fn login_during_media_does_not_cancel_media() {
        // The session lifecycle cycling (as /login does) can't reach the
        // media path: no CancelMedia, no ClearBridge, turn untouched.
        let mut sim = Sim::media_over_paused_baseline();
        let down = sim.step(Input::LinkDown { gen: 1 });
        sim.advance(Duration::from_secs(2));
        let up = sim.step(Input::LinkUp { gen: 2 });
        for fx in [&down, &up] {
            assert!(!has_cancel(fx) && !has_clear(fx));
        }
        assert!(matches!(sim.s.active, Active::Media { .. }));
        assert_eq!(sim.s.link_gen, 2);
    }

    #[test]
    fn phone_play_during_media_pauses_spotify_and_keeps_card_suppressed() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::Playing { uri: uri(9), meta: Some(meta("x")) });
        assert_eq!(spircs(&fx), vec![SpircCmd::Pause]);
        assert!(!has_spotify_card(&fx) && !has_presence(&fx));
        assert!(matches!(sim.s.active, Active::Media { .. }), "no incoming event moves the turn");
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForMedia), "it airs after the queue");
    }

    #[test]
    fn armed_track_never_queued_twice() {
        // Lost is advisory: a slow ack plus a blind retry would queue the
        // track twice, and librespot has no dequeue.
        let mut sim = Sim::baseline_playing();
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(add_to_queue_count(&fx), 1);

        sim.advance(Duration::from_millis(2500));
        let fx = sim.step(Input::Tick(TimerKind::ArmAck));
        assert_eq!(add_to_queue_count(&fx), 0, "Lost never blind-retries");
        assert!(matches!(sim.s.armed, Some(Armed { ack: Ack::Lost, .. })));

        // The slow ack finally lands: recovered, still exactly one queue.
        let fx = sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![uri(1)],
            context_uri: None,
        });
        assert_eq!(add_to_queue_count(&fx), 0);
        assert!(matches!(sim.s.armed, Some(Armed { ack: Ack::Confirmed, .. })));
    }

    #[test]
    fn end_of_track_does_not_clear_armed() {
        // Armed clears on exactly one predicate family (its own Playing, a
        // phone deletion, link-down snapshot, void-by-disconnect, stop) —
        // never on a boundary.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.transport(TransportEvent::EndOfTrack);
        assert!(matches!(sim.s.armed, Some(Armed { ack: Ack::Confirmed, .. })));
        assert_eq!(sim.s.sp, SpDevice::Boundary, "a boundary, never Idle");
    }

    #[test]
    fn skipping_a_media_item_after_a_stop_resumes_the_spotify_head() {
        // Live E': a pause the bot did not own for the media item once
        // froze the next skip — the media item's ⏭ honoured it and nothing
        // played. A human skip is an advance regardless of who paused.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        sim.s.queue.push(media_item("m"));
        let item = sim.s.queue.pop().unwrap();
        sim.s.media_epoch = 1;
        sim.s.active = Active::Media { item, paused: false, epoch: 1 };
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.skip();
        let fx = sim.media_ended(MediaOutcome::Cancelled);
        // Cued track (9) isn't the head: advance onto the confirmed head.
        assert_eq!(spircs(&fx), vec![SpircCmd::Next, SpircCmd::Play], "the skip advances: {fx:?}");
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
    }

    #[test]
    fn takeover_on_a_fresh_session_loads_the_spotify_head_at_once() {
        // Live I': ▶ activated the device but left it Inactive, so the
        // second ▶ sent Play to a device with nothing loaded.
        let mut sim = Sim::new();
        sim.s.sp = SpDevice::Inactive;
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice, SpircCmd::Load(uri(1))]);
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
    }

    #[test]
    fn bare_play_never_pauses() {
        let mut sim = Sim::baseline_playing();
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Play { reply: tx });
        assert!(spircs(&fx).is_empty(), "audible: refused, no Pause");
        // Paused baseline: it is ⏯'s ▶ half.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Play { reply: tx });
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
    }

    #[test]
    fn playing_without_meta_resolves_instead_of_posting_an_unknown_card() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::Playing { uri: uri(2), meta: None });
        assert!(!fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::NowPlayingSpotify { .. }))));
        assert!(fx.iter().any(|e| matches!(e, Effect::ResolveMeta(u) if *u == uri(2))));
        // The resolved metadata arrives as TrackChanged and posts the card.
        let meta = TrackMeta { title: "t".into(), artist: "a".into(), album_art_url: None };
        let fx = sim.transport(TransportEvent::TrackChanged { uri: uri(2), meta });
        assert!(fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::NowPlayingSpotify { .. }))));
    }

    #[test]
    fn spotify_pending_times_out() {
        // An F2-dropped Load must not park the player: retry once while
        // idle, then surface an error and free the turn.
        let mut sim = Sim::idle_device();
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::Load(uri(1))]);

        sim.advance(Duration::from_millis(5100));
        let fx = sim.step(Input::Tick(TimerKind::SpotifyPending));
        assert_eq!(spircs(&fx), vec![SpircCmd::Load(uri(1))], "one retry while idle");
        assert!(timer_set(&fx, TimerKind::SpotifyPending));

        sim.advance(Duration::from_millis(5100));
        let fx = sim.step(Input::Tick(TimerKind::SpotifyPending));
        assert!(spircs(&fx).is_empty(), "never a second retry");
        assert!(has_notice(&fx));
        assert!(matches!(sim.s.active, Active::None));
    }

    #[test]
    fn spotify_pending_timeout_while_inactive_prompts_takeover() {
        let mut sim = Sim::idle_device();
        sim.push_spotify(1, "s1");
        sim.toggle();
        sim.s.device_active = false; // lost the device while waiting
        sim.advance(Duration::from_millis(5100));
        let fx = sim.step(Input::Tick(TimerKind::SpotifyPending));
        assert!(has_takeover_prompt(&fx));
        assert!(matches!(sim.s.active, Active::None));
    }

    #[test]
    fn stop_keeps_the_queue_and_leaves_the_channel() {
        // Stop is stop, not clear: /clear is the only thing that empties.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.stop();

        assert_eq!(sim.s.queue.len(), 1, "the request survives a stop");
        assert!(has_leave(&fx), "the bot leaves the channel");
        assert_eq!(spircs(&fx), vec![SpircCmd::Disconnect]);
        assert!(!sim.s.device_active, "the Connect device is released");
        assert!(matches!(sim.s.voice, VoiceStatus::Down));
        assert!(reply_text(&fx).contains("kept"), "got: {}", reply_text(&fx));
    }

    #[test]
    fn a_bare_play_after_a_stop_brings_the_bot_back_into_voice() {
        // Live 2026-09-01: `/stop` leaves the channel but keeps the session,
        // so a bare `/play` is how you restart. It re-claimed the Connect
        // device and replied "▶ Taking over the Spotify device" without
        // rejoining, so the next track decoded into a bridge no call was
        // draining — the card said playing, the channel was silent, and
        // only `/login` brought the bot back.
        let mut sim = Sim::baseline_playing();
        sim.stop();
        sim.transport(TransportEvent::SessionDisconnected);
        assert!(matches!(sim.s.voice, VoiceStatus::Down));
        assert!(!sim.s.device_active, "the stop released the device");

        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Play { reply: tx });
        assert!(has_join(&fx), "the takeover rejoins the channel: {fx:?}");
        assert!(matches!(sim.s.voice, VoiceStatus::Joining));
        assert!(sim.s.device_active, "and still claims the device");
    }

    #[test]
    fn spotify_taking_the_turn_with_voice_down_joins_the_channel() {
        // The other half of the same hole: the DJ presses play on their
        // phone after a takeover. Our librespot decodes, so audio exists —
        // it needs a call to drain into, whoever started it.
        let mut sim = Sim::idle_device();
        sim.s.voice = VoiceStatus::Down;
        let fx = sim.transport(TransportEvent::Playing { uri: uri(9), meta: None });
        assert!(has_join(&fx), "audio with no call is silence: {fx:?}");
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
    }

    #[test]
    fn resuming_a_paused_baseline_with_an_empty_queue_joins_voice() {
        // ⏯'s ▶ half with nothing queued: a resume is still audio.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        sim.s.voice = VoiceStatus::Down;
        let fx = sim.toggle();
        assert!(has_join(&fx), "{fx:?}");
        assert!(spircs(&fx).contains(&SpircCmd::Play));
    }

    #[test]
    fn a_refused_play_never_joins_voice() {
        // The one ▶ outcome that makes no sound must not drag the bot into
        // a channel: nothing is queued and the device is idle, so there is
        // nothing to resume.
        let mut sim = Sim::idle_device();
        sim.s.voice = VoiceStatus::Down;
        let fx = sim.toggle();
        assert!(!has_join(&fx), "nothing to play: no join: {fx:?}");
        assert!(reply_text(&fx).contains("Nothing is playing"), "{}", reply_text(&fx));
    }

    #[test]
    fn teardown_stop_never_asks_to_leave_voice() {
        // Live 2026-09-01: a force disconnect ran the teardown, whose Stop
        // emitted LeaveVoice; the shell armed its deliberate-leave guard and
        // removed a call Discord had already dropped, so no echo ever
        // consumed the guard and the next force disconnect would have been
        // read as deliberate. Teardown owns voice itself — the core must not.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.teardown();

        assert!(!has_leave(&fx), "voice is already gone: nothing to leave");
        assert!(has_clear(&fx), "the bridge is still silenced");
        assert_eq!(spircs(&fx), vec![SpircCmd::Disconnect], "the device is still released");
        assert!(!sim.s.device_active);
        assert!(matches!(sim.s.active, Active::None));
        assert!(matches!(sim.s.voice, VoiceStatus::Down));
        assert_eq!(sim.s.queue.len(), 1, "the queue survives a teardown too");
    }

    #[test]
    fn stop_drops_the_arm_because_the_disconnect_resets_the_device() {
        // `Disconnect` resets librespot's device state, queue included, so a
        // surviving arm is a ghost: `maybe_arm` refuses to re-arm behind one,
        // and the next `SetQueue` reads its absence as "deleted on the phone"
        // and drops the very request the stop promised to keep.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.stop();
        assert!(sim.s.armed.is_none(), "the arm cannot outlive the device");
        assert_eq!(sim.s.queue.len(), 1, "but the request itself is kept");
    }

    #[test]
    fn media_end_while_link_down_resumes_after_reconnect() {
        let mut sim = Sim::media_over_paused_baseline();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);

        sim.step(Input::LinkDown { gen: 1 });
        assert!(sim.s.armed.is_none(), "a Confirmed ghost would wedge arming forever");

        let fx = sim.media_ended(MediaOutcome::Finished);
        assert!(spircs(&fx).is_empty(), "no one to talk to while the link is down");
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForMedia), "the resume debt is kept");
        assert!(matches!(sim.s.active, Active::None));

        sim.advance(Duration::from_secs(30));
        let fx = sim.step(Input::LinkUp { gen: 2 });
        assert_eq!(
            spircs(&fx),
            vec![SpircCmd::Transfer, SpircCmd::Play],
            "restore the device state, then pay the resume debt"
        );
        assert!(
            matches!(sim.s.armed, Some(Armed { ack: Ack::Sent(_), .. })),
            "the restored arm awaits re-confirmation by the post-transfer SetQueue"
        );
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
    }

    #[test]
    fn takeover_stopped_is_not_idle() {
        // A takeover emits SessionDisconnected then Stopped; that Stopped
        // must not enable load() over the device that took over.
        let mut sim = Sim::media_over_paused_baseline();
        sim.push_spotify(1, "s1");
        sim.transport(TransportEvent::SessionDisconnected);
        sim.transport(TransportEvent::Stopped);
        assert_eq!(sim.s.sp, SpDevice::Inactive, "Inactive, not Idle");
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert!(spircs(&fx).is_empty(), "no Load over another device's playback");
        assert!(matches!(sim.s.active, Active::None));
    }

    // --- bridge ownership: only Spotify's own turn clears it ---------------
    //
    // The shared-bridge hazard: `AudioBridge` is also written by the media
    // feeder and the DJ overlay. These mirror the four raw
    // `bridge.clear()` calls deleted from `spotify/player.rs` as gated
    // `Effect::ClearBridge` emissions here — they must fire when Spotify's
    // own transport reports it stopped being audible, and must never fire
    // while a media item holds the turn (that was the bug: a librespot
    // reconnect or a phone-side pause wiping a YouTube track mid-play).

    #[test]
    fn paused_does_not_clear_the_bridge_while_media_holds_the_turn() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::Paused { uri: uri(9) });
        assert!(!has_clear(&fx));
    }

    #[test]
    fn paused_clears_the_bridge_while_spotify_holds_the_turn() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::Paused { uri: uri(9) });
        assert!(has_clear(&fx));
    }

    #[test]
    fn stopped_does_not_clear_the_bridge_while_media_holds_the_turn() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::Stopped);
        assert!(!has_clear(&fx));
    }

    #[test]
    fn stopped_clears_the_bridge_while_spotify_holds_the_turn() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::Stopped);
        assert!(has_clear(&fx));
    }

    #[test]
    fn unavailable_does_not_clear_the_bridge_while_media_holds_the_turn() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::Unavailable { uri: uri(9) });
        assert!(!has_clear(&fx));
    }

    #[test]
    fn unavailable_clears_the_bridge_while_spotify_holds_the_turn() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::Unavailable { uri: uri(9) });
        assert!(has_clear(&fx));
    }

    #[test]
    fn unavailable_for_a_preloaded_track_leaves_the_playing_one_alone() {
        // Seen live: librespot failed the key fetch for the track it was
        // preloading and reported it unavailable while a different track
        // played on. Clearing there cut the playing track's buffer.
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::Unavailable { uri: uri(7) });
        assert!(!has_clear(&fx));
        // Still surfaced to the channel — it just doesn't touch the audio.
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::Ui(UiMsg::Notice(_)))));
    }

    #[test]
    fn link_down_does_not_clear_the_bridge_while_media_holds_the_turn() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.step(Input::LinkDown { gen: 1 });
        assert!(!has_clear(&fx));
    }

    #[test]
    fn link_down_clears_the_bridge_while_spotify_holds_the_turn() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.step(Input::LinkDown { gen: 1 });
        assert!(has_clear(&fx));
    }

    #[test]
    fn link_reconnecting_does_not_clear_the_bridge_while_media_holds_the_turn() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.step(Input::LinkReconnecting { gen: 1 });
        assert!(!has_clear(&fx));
    }

    // --- Playing bookkeeping ----------------------------------------------

    #[test]
    fn playing_pops_a_matching_queued_item_anywhere() {
        // Today's behaviour, kept: a track the DJ's own playlist reaches
        // isn't also aired later as a request.
        let mut sim = Sim::baseline_playing();
        sim.s.queue.push(media_item("m"));
        sim.push_spotify(2, "s2");
        sim.transport(TransportEvent::Playing { uri: uri(2), meta: None });
        assert_eq!(sim.s.queue.len(), 1);
        assert!(
            matches!(head_of(&sim.s.queue), Head::Media),
            "the media item is untouched; the matching request is consumed"
        );
    }

    #[test]
    fn playing_the_armed_track_pops_it_and_rearms_the_next() {
        // Gapless Spotify→Spotify: the next arm goes out a whole track
        // ahead of the preload window.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.push_spotify(2, "s2");
        let fx = sim.transport(TransportEvent::Playing { uri: uri(1), meta: Some(meta("s1")) });
        assert_eq!(sim.s.queue.len(), 1);
        assert!(matches!(&sim.s.armed, Some(Armed { uri: u, .. }) if *u == uri(2)));
        assert_eq!(add_to_queue_count(&fx), 1);
        assert!(has_spotify_card(&fx));
    }

    #[test]
    fn pause_echo_within_window_is_not_a_human_pause() {
        let mut sim = Sim::baseline_playing();
        sim.s.queue.push(media_item("m"));
        sim.skip(); // sends Pause, records it inflight
        sim.transport(TransportEvent::Paused { uri: uri(9) });
        assert_eq!(
            sim.s.pause_owner,
            Some(PauseOwner::BotForMedia),
            "our own echo keeps the bot's ownership"
        );
    }

    #[test]
    fn human_pause_before_media_blocks_the_auto_resume() {
        // Live smoke 8: phone-pause before a media item → media ends → the
        // baseline STAYS paused.
        let mut sim = Sim::baseline_playing();
        sim.transport(TransportEvent::Paused { uri: uri(9) });
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::Human));
        sim.s.queue.push(media_item("m"));
        let fx = sim.toggle(); // ▶ starts the media head, baseline untouched
        assert!(has_start_media(&fx));
        assert!(spircs(&fx).is_empty());
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert!(spircs(&fx).is_empty(), "the phone's pause is honoured");
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::Human));
    }

    fn prev_track(id: i64, n: u64, context: Option<&str>) -> PreviousTrack {
        PreviousTrack {
            id,
            uri: uri(n),
            context_uri: context.map(|c| c.to_string()),
        }
    }

    #[test]
    fn previous_asks_the_history_rather_than_spotify() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.previous();
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::ResolvePrevious { before: None })),
            "walks from live the first time"
        );
        assert!(spircs(&fx).is_empty(), "nothing is sent until it resolves");
    }

    #[test]
    fn a_resolved_previous_reopens_the_playlist_at_that_track() {
        let mut sim = Sim::baseline_playing();
        sim.previous();
        let fx = sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });

        assert_eq!(
            spircs(&fx),
            vec![SpircCmd::LoadContext {
                context_uri: "spotify:playlist:abc".to_string(),
                track_uri: uri(3),
                options: PlayOptions::default(),
            }],
            "reopens the context instead of replacing it with one track"
        );
        assert_eq!(sim.s.history_cursor, Some(7), "the cursor advances back");
    }

    #[test]
    fn a_back_jump_leaves_an_armed_request_armed() {
        // The whole promise of going back is that it costs you nothing:
        // whoever queued a track still hears it after the detour. Librespot
        // keeps its queue across a context load (`clear_next_tracks` is
        // queue-preserving), so the arm is still live on Spotify's side —
        // dropping or re-issuing it here would either lose the request or
        // queue it twice, and librespot has no dequeue to undo that with.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);

        sim.previous();
        let fx = sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });

        assert_eq!(
            spircs(&fx),
            vec![SpircCmd::LoadContext {
                context_uri: "spotify:playlist:abc".to_string(),
                track_uri: uri(3),
                options: PlayOptions::default(),
            }],
            "the jump alone — no re-arm, and nothing that would double-queue"
        );
        let armed = sim.s.armed.as_ref().expect("the request stays armed");
        assert_eq!(armed.item_id, id);
        assert_eq!(armed.uri, uri(1));
        assert!(matches!(armed.ack, Ack::Confirmed), "and stays acked");
        assert_eq!(sim.s.queue.len(), 1, "and stays in the queue");
    }

    #[test]
    fn the_armed_request_still_airs_after_the_jumped_to_track() {
        // The other half: the detour ends and the request is next, exactly
        // as it would have been without it.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.previous();
        sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });
        // The jump lands.
        sim.transport(TransportEvent::Playing { uri: uri(3), meta: None });
        assert!(sim.s.armed.is_some(), "landing on the jump is not the request");
        assert_eq!(sim.s.queue.len(), 1);

        // Spotify's own advance reaches the armed track.
        sim.transport(TransportEvent::EndOfTrack);
        sim.transport(TransportEvent::Playing { uri: uri(1), meta: None });
        assert!(sim.s.armed.is_none(), "the arm is consumed by its own track");
        assert_eq!(sim.s.queue.len(), 0, "and the request leaves the queue");
    }

    #[test]
    fn a_back_jump_hands_the_djs_shuffle_and_repeat_back() {
        // Loading a context resets these on librespot's side, so the jump
        // has to restore them or it silently turns the DJ's shuffle off.
        let mut sim = Sim::baseline_playing();
        sim.transport(TransportEvent::OptionsChanged {
            shuffle: true,
            repeat_context: true,
            repeat_track: false,
        });
        sim.previous();
        let fx = sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });

        assert_eq!(
            spircs(&fx),
            vec![SpircCmd::LoadContext {
                context_uri: "spotify:playlist:abc".to_string(),
                track_uri: uri(3),
                options: PlayOptions {
                    shuffle: true,
                    repeat_context: true,
                    repeat_track: false,
                },
            }]
        );
    }

    #[test]
    fn shuffle_and_repeat_arrive_separately_but_are_tracked_together() {
        // Spotify sends each half in its own event; neither may clobber the
        // other's value.
        let mut sim = Sim::baseline_playing();
        sim.transport(TransportEvent::OptionsChanged {
            shuffle: true,
            repeat_context: false,
            repeat_track: false,
        });
        sim.transport(TransportEvent::OptionsChanged {
            shuffle: true,
            repeat_context: false,
            repeat_track: true,
        });
        assert_eq!(
            sim.s.play_options,
            PlayOptions { shuffle: true, repeat_context: false, repeat_track: true }
        );
    }

    #[test]
    fn a_track_with_no_recorded_context_falls_back_to_spotifys_own_previous() {
        let mut sim = Sim::baseline_playing();
        sim.previous();
        let fx = sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, None)),
        });
        assert_eq!(spircs(&fx), vec![SpircCmd::Previous]);
    }

    #[test]
    fn a_context_less_back_jump_still_walks_backwards() {
        // The fallback hands the step to Spotify, so it names no track — but
        // it must still move the walk, or the next press re-anchors on the
        // newest row (the one this jump is about to write) and lands on the
        // track we started from.
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.step(Input::PreviousResolved { track: Some(prev_track(7, 3, None)) });
        assert_eq!(sim.s.history_cursor, None, "not committed until it lands");

        sim.transport(TransportEvent::Playing { uri: uri(3), meta: Some(meta("Third")) });
        assert_eq!(
            sim.s.history_cursor,
            Some(7),
            "a different track arrived, so the step really happened"
        );
    }

    #[test]
    fn a_back_jump_that_only_seeks_to_zero_leaves_the_walk_where_it_was() {
        // F16: librespot's `Previous` steps back only under 3 s into a
        // track; at or over 3 s it seeks to zero and keeps playing the same
        // one. Re-emitting the same track is not a step, so the walk must
        // not claim a move it did not make. This is what stops the
        // `moved` check being "simplified" into an unconditional commit,
        // which would leave the cursor a whole track ahead of the truth.
        let mut sim = Sim::baseline_playing();
        sim.s.history_cursor = Some(11);
        sim.previous();
        sim.step(Input::PreviousResolved { track: Some(prev_track(7, 9, None)) });

        sim.transport(TransportEvent::Playing { uri: uri(9), meta: None });
        assert_eq!(
            sim.s.history_cursor,
            Some(11),
            "same track back at 0:00 — the walk stays put"
        );
    }

    #[test]
    fn reaching_the_start_of_the_history_says_so() {
        let mut sim = Sim::baseline_playing();
        sim.previous();
        let fx = sim.step(Input::PreviousResolved { track: None });
        assert!(spircs(&fx).is_empty());
        assert!(reply_text(&fx).contains("Nothing further back"));
    }

    #[test]
    fn a_jump_that_lands_on_the_wrong_track_is_reported_not_swallowed() {
        // Librespot starts a context at track 1 when the requested track
        // isn't in it, and only logs about it — so we check the arrival.
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });

        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(99),
            meta: Some(meta("Not What Was Asked For")),
        });
        assert!(
            fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::Notice(_)))),
            "the mismatch surfaces"
        );
        assert_eq!(sim.s.history_cursor, None, "and the walk is abandoned");
    }

    #[test]
    fn the_playlist_moving_on_puts_us_back_live() {
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });
        // The jump lands.
        sim.transport(TransportEvent::Playing { uri: uri(3), meta: Some(meta("jumped-to")) });
        assert_eq!(sim.s.history_cursor, Some(7), "still positioned there");

        // The context then advances on its own: no longer browsing history.
        sim.transport(TransportEvent::Playing { uri: uri(4), meta: Some(meta("next")) });
        assert_eq!(sim.s.history_cursor, None);
    }

    #[test]
    fn a_second_back_press_waits_instead_of_stealing_the_first_reply() {
        // Overwriting `pending_reply` would drop the first caller's channel
        // (their interaction times out) and walk back twice for one intent.
        let mut sim = Sim::baseline_playing();
        let first = sim.previous();
        assert!(first
            .iter()
            .any(|e| matches!(e, Effect::ResolvePrevious { .. })));

        let second = sim.previous();
        assert!(
            !second
                .iter()
                .any(|e| matches!(e, Effect::ResolvePrevious { .. })),
            "the second press does not start a second walk"
        );
        assert!(reply_text(&second).contains("Already going back"));
    }

    #[test]
    fn a_media_item_records_the_reference_its_own_kind_is_played_from() {
        // The feeder fetches the url; back-navigation parses the ref. A
        // swapped field here is junk in the history and unplayable later.
        let mut sim = Sim::new();
        sim.s.voice = VoiceStatus::Ready;

        let mut fx = Vec::new();
        start_media(&mut sim.s, media_item("a-track"), StartGate::Immediate, &mut fx);
        let rows = aired(&fx);
        // The fixture's url is "u", its title "a-track" and its id "v", so
        // this distinguishes the url field from every neighbour it could be
        // confused with.
        assert_eq!(rows[0].track_ref, "u", "a YouTube item records its url");

        let file = QueueItem::new(
            MediaSource::File {
                filename: "clip.mp3".into(),
                attachment_url: "https://cdn.discord/attachment".into(),
            },
            "Papos".into(),
            1,
        );
        let mut fx = Vec::new();
        start_media(&mut sim.s, file, StartGate::Immediate, &mut fx);
        let rows = aired(&fx);
        assert_eq!(
            rows[0].track_ref, "https://cdn.discord/attachment",
            "a file item records the url it is fetched from, not its name"
        );
    }

    #[test]
    fn resuming_the_jumped_to_track_does_not_throw_the_walk_away() {
        // Librespot re-emits Playing on a resume or seek. Treating that as
        // "the playlist moved on" reset the cursor, bringing back the very
        // two-track bounce the cursor exists to prevent.
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });
        sim.transport(TransportEvent::Playing { uri: uri(3), meta: Some(meta("jumped-to")) });
        assert_eq!(sim.s.history_cursor, Some(7));

        // Pause, resume: the same track plays again.
        sim.transport(TransportEvent::Playing { uri: uri(3), meta: Some(meta("jumped-to")) });
        assert_eq!(sim.s.history_cursor, Some(7), "a resume is not movement");
    }

    #[test]
    fn a_jump_resolved_after_a_stop_is_dropped_rather_than_replayed() {
        // The history read is asynchronous. A /stop in the meantime released
        // the device and left the channel; loading a context then would
        // re-claim the device and play from outside the call.
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.stop();
        let fx = sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });
        assert!(spircs(&fx).is_empty(), "nothing is sent to a released device");
    }

    #[test]
    fn a_pending_jump_does_not_outlive_the_session() {
        // Left set, a stale marker makes some later unrelated track report a
        // mismatch for a jump nobody remembers asking for.
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });
        sim.transport(TransportEvent::SessionDisconnected);

        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(99),
            meta: Some(meta("something much later")),
        });
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Ui(UiMsg::Notice(_)))),
            "no warning for a jump the session already cancelled"
        );
    }

    #[test]
    fn the_failed_jump_notice_reads_as_one_sentence() {
        // A rustfmt line-join once left a 26-space run mid-sentence in this
        // string, and it shipped to the channel that way.
        let mut sim = Sim::baseline_playing();
        sim.previous();
        sim.step(Input::PreviousResolved {
            track: Some(prev_track(7, 3, Some("spotify:playlist:abc"))),
        });
        let fx = sim.transport(TransportEvent::Playing { uri: uri(99), meta: Some(meta("x")) });
        let notice = fx
            .iter()
            .find_map(|e| match e {
                Effect::Ui(UiMsg::Notice(t)) => Some(t.clone()),
                _ => None,
            })
            .expect("a notice");
        assert!(!notice.contains("  "), "double space in: {notice:?}");
    }

    #[test]
    fn previous_still_refuses_during_a_queue_item() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.previous();
        assert!(!fx
            .iter()
            .any(|e| matches!(e, Effect::ResolvePrevious { .. })));
        assert!(reply_text(&fx).contains("isn't available"));
    }

    #[test]
    fn clearing_empties_the_queue_and_leaves_playback_alone() {
        // The whole difference from /stop: nothing goes quiet.
        let mut sim = Sim::baseline_playing();
        sim.enqueue(media_item("a"));
        sim.enqueue(media_item("b"));

        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::ClearQueue { reply: tx });

        assert_eq!(sim.s.queue.len(), 0);
        assert!(
            matches!(sim.s.active, Active::Spotify { .. }),
            "the baseline keeps the turn"
        );
        assert!(spircs(&fx).is_empty(), "clearing sends Spotify nothing");
        assert!(!has_clear(&fx), "and never touches the bridge");
    }

    #[test]
    fn clearing_warns_when_a_track_is_already_committed_to_spotify() {
        // Librespot has no dequeue, so an armed track still airs once.
        let mut sim = Sim::baseline_playing();
        sim.enqueue(spotify_item(1, "s1"));
        assert!(sim.s.armed.is_some(), "precondition: something is armed");

        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::ClearQueue { reply: tx });

        assert!(sim.s.armed.is_none());
        let text = reply_text(&fx);
        assert!(text.contains("still play once"), "got: {text}");
    }

    #[test]
    fn clearing_an_empty_queue_says_so_rather_than_claiming_a_clear() {
        let mut sim = Sim::baseline_playing();
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::ClearQueue { reply: tx });
        assert_eq!(reply_text(&fx), "The queue is already empty.");
    }

    #[test]
    fn restoring_refills_the_queue_without_playing_anything() {
        let mut sim = Sim::new();
        let fx = sim.step(Input::RestoreQueue {
            items: vec![media_item("a"), media_item("b")],
        });

        assert_eq!(sim.s.queue.len(), 2);
        assert!(matches!(sim.s.active, Active::None), "restoring is silent");
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::StartMedia { .. })),
            "a restored queue must not start itself"
        );
    }

    #[test]
    fn restoring_stamps_fresh_ids_rather_than_reusing_stored_ones() {
        let mut sim = Sim::new();
        sim.step(Input::RestoreQueue { items: vec![media_item("a"), media_item("b")] });
        let ids: Vec<u64> = sim.s.queue.snapshot().iter().map(|i| i.item_id).collect();
        assert_eq!(ids, vec![1, 2], "an id names a residency, not a track");
    }

    #[test]
    fn restoring_never_overwrites_a_queue_someone_is_already_using() {
        // The restore is asynchronous at boot; anything queued by a person
        // in the meantime is newer than the record and outranks it.
        let mut sim = Sim::new();
        sim.enqueue(media_item("queued-by-a-human"));
        sim.step(Input::RestoreQueue { items: vec![media_item("from-disk")] });

        assert_eq!(sim.s.queue.len(), 1);
        assert_eq!(
            sim.s.queue.peek().unwrap().source.display_title(),
            "queued-by-a-human"
        );
    }

    #[test]
    fn an_armed_request_is_recorded_without_a_context() {
        // A request reaches the device through Spotify's queue, not through
        // the playlist that happens to be loaded. Stamping that playlist on
        // its row makes a later ⏮ ask for a track the context does not hold,
        // which librespot answers by restarting the context from the top.
        let mut sim = Sim::baseline_playing();
        sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![],
            context_uri: Some("spotify:playlist:abc".into()),
        });
        let id = sim.push_spotify(4, "requested");
        sim.arm(4, id, Ack::Confirmed);

        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(4),
            meta: Some(meta("Requested")),
        });

        let rows = aired(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, AiredSource::Request);
        assert_eq!(rows[0].context_uri, None, "it was never in that playlist");
    }

    #[test]
    fn a_track_the_playlist_reaches_keeps_its_context_even_though_it_was_queued() {
        // The narrow half of the same rule: the pop also fires when the DJ's
        // own context reaches a track someone had queued. That airing really
        // is inside the context, so it keeps it.
        let mut sim = Sim::baseline_playing();
        sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![],
            context_uri: Some("spotify:playlist:abc".into()),
        });
        sim.push_spotify(5, "also-queued");

        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(5),
            meta: Some(meta("Also queued")),
        });

        let rows = aired(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, AiredSource::Request, "someone did queue it");
        assert_eq!(
            rows[0].context_uri.as_deref(),
            Some("spotify:playlist:abc"),
            "but the playlist is what reached it"
        );
    }

    #[test]
    fn a_request_that_surfaces_under_a_media_turn_still_names_its_requester() {
        // Spotify starts the armed request while a media item holds the
        // turn: it is popped there (it must be, or it could be armed twice)
        // and paused straight back down. The airing history records is the
        // later one, and it has to carry who asked for it.
        let mut sim = Sim::media_over_paused_baseline();
        let id = sim.push_spotify(4, "requested");
        sim.arm(4, id, Ack::Confirmed);

        let early = sim.transport(TransportEvent::Playing {
            uri: uri(4),
            meta: Some(meta("Requested")),
        });
        assert!(aired(&early).is_empty(), "it is not audible yet");
        assert!(sim.s.pending_request.is_some(), "its requester is held");

        sim.media_ended(MediaOutcome::Finished);
        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(4),
            meta: Some(meta("Requested")),
        });

        let rows = aired(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, AiredSource::Request);
        assert_eq!(rows[0].queued_by.as_deref(), Some("dj"));
        assert!(sim.s.pending_request.is_none(), "and consumed once");
    }

    fn aired(fx: &[Effect]) -> Vec<&AiredTrack> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::RecordAired(a) => Some(a),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_baseline_track_is_recorded_with_the_context_it_aired_from() {
        let mut sim = Sim::baseline_playing();
        sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![],
            context_uri: Some("spotify:playlist:abc".into()),
        });

        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(3),
            meta: Some(meta("Something")),
        });

        let rows = aired(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, AiredSource::Baseline, "nobody queued it");
        assert_eq!(rows[0].track_ref, uri(3).to_string());
        assert_eq!(rows[0].context_uri.as_deref(), Some("spotify:playlist:abc"));
        assert_eq!(rows[0].queued_by, None);
    }

    #[test]
    fn an_aired_request_records_who_asked_for_it() {
        // Librespot sends TrackChanged and THEN Playing for the same track,
        // and the card (so the history row) is written at the first of the
        // two. Driving only Playing hid a bug where every Spotify track was
        // logged as the baseline with no requester.
        let mut sim = Sim::baseline_playing();
        sim.enqueue(spotify_item(1, "s1"));

        let fx = sim.transport(TransportEvent::TrackChanged {
            uri: uri(1),
            meta: meta("s1"),
        });
        let rows = aired(&fx);
        assert_eq!(rows.len(), 1, "recorded when the card goes up");
        assert_eq!(rows[0].source, AiredSource::Request, "it was queued here");
        assert!(rows[0].queued_by.is_some(), "and names who asked");

        // The Playing that follows pops the request but must not log it twice.
        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(1),
            meta: Some(meta("s1")),
        });
        assert!(aired(&fx).is_empty(), "one row per airing");
    }

    #[test]
    fn a_baseline_track_is_still_recorded_as_baseline_in_event_order() {
        // The mirror of the above: nothing queued, so nobody asked for it.
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::TrackChanged {
            uri: uri(3),
            meta: meta("whatever the playlist reached"),
        });
        let rows = aired(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, AiredSource::Baseline);
        assert_eq!(rows[0].queued_by, None);
    }

    #[test]
    fn the_same_track_repeating_playing_is_only_recorded_once() {
        // Librespot re-emits Playing on a seek or a resume; history is a log
        // of airings, not of transport events.
        let mut sim = Sim::baseline_playing();
        let first = sim.transport(TransportEvent::Playing {
            uri: uri(3),
            meta: Some(meta("Something")),
        });
        assert_eq!(aired(&first).len(), 1);

        let again = sim.transport(TransportEvent::Playing {
            uri: uri(3),
            meta: Some(meta("Something")),
        });
        assert_eq!(aired(&again).len(), 0, "same track, no second row");
    }

    #[test]
    fn a_spotify_play_under_a_media_item_records_nothing() {
        // It never became audible — the actor pauses it straight back down.
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::Playing {
            uri: uri(3),
            meta: Some(meta("Something")),
        });
        assert!(aired(&fx).is_empty());
    }

    #[test]
    fn starting_a_media_item_records_it_as_a_request() {
        let mut sim = Sim::new();
        sim.s.voice = VoiceStatus::Ready;
        let mut fx = Vec::new();
        start_media(&mut sim.s, media_item("a-track"), StartGate::Immediate, &mut fx);

        let rows = aired(&fx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, AiredSource::Request);
        assert_eq!(rows[0].context_uri, None, "media has no Spotify context");
        assert!(rows[0].queued_by.is_some());
    }

    #[test]
    fn set_queue_remembers_the_context_and_keeps_it_across_silent_ones() {
        let mut sim = Sim::baseline_playing();
        assert_eq!(sim.s.context_uri, None);

        sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![],
            context_uri: Some("spotify:playlist:abc".into()),
        });
        assert_eq!(sim.s.context_uri.as_deref(), Some("spotify:playlist:abc"));

        // Spotify names the context only on a load or queue mutation, so a
        // later event without one must not erase what we know.
        sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![],
            context_uri: None,
        });
        assert_eq!(sim.s.context_uri.as_deref(), Some("spotify:playlist:abc"));

        // A genuine context change replaces it.
        sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![],
            context_uri: Some("spotify:album:xyz".into()),
        });
        assert_eq!(sim.s.context_uri.as_deref(), Some("spotify:album:xyz"));
    }

    // --- SetQueue ack machine ---------------------------------------------

    #[test]
    fn set_queue_confirms_a_sent_arm() {
        let mut sim = Sim::baseline_playing();
        sim.enqueue(spotify_item(1, "s1")); // arms, Sent
        let fx = sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![uri(1)],
            context_uri: None,
        });
        assert!(fx.is_empty());
        assert!(matches!(sim.s.armed, Some(Armed { ack: Ack::Confirmed, .. })));
    }

    #[test]
    fn set_queue_deletion_pops_the_item_and_rearms_the_next() {
        // Deleting the request on the phone is a cancel.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.push_spotify(2, "s2");
        let fx = sim.transport(TransportEvent::SetQueue { current: Some(uri(9)), queued: vec![], context_uri: None });
        assert_eq!(sim.s.queue.len(), 1, "the cancelled request is dropped");
        assert!(matches!(&sim.s.armed, Some(Armed { uri: u, .. }) if *u == uri(2)));
        assert_eq!(add_to_queue_count(&fx), 1, "the next item arms in its place");
        assert!(has_notice(&fx));
    }

    #[test]
    fn set_queue_absence_while_current_is_not_a_deletion() {
        // Absent from next_tracks BUT equal to current_track: auto-advance
        // just landed on the armed track — the exact misfire the
        // current-track check exists for.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.transport(TransportEvent::SetQueue { current: Some(uri(1)), queued: vec![], context_uri: None });
        assert!(fx.is_empty());
        assert!(sim.s.armed.is_some());
        assert_eq!(sim.s.queue.len(), 1);
    }

    // --- staleness guards -------------------------------------------------

    #[test]
    fn stale_generation_transport_is_ignored() {
        let mut sim = Sim::baseline_playing();
        let fx = sim.step(Input::Transport {
            gen: 7,
            ev: TransportEvent::Playing { uri: uri(2), meta: None },
        });
        assert!(fx.is_empty());
        assert_eq!(sim.s.sp, SpDevice::Playing(uri(9)), "a dead session can't touch the mirror");
    }

    #[test]
    fn stale_media_epoch_is_ignored() {
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.queue.push(media_item("next"));
        sim.skip(); // cancels epoch 1; active stays Media until the report
        let fx = sim.media_ended(MediaOutcome::Cancelled); // starts "next" as epoch 2
        assert!(has_start_media(&fx));
        let fx = sim.step(Input::MediaEnded { epoch: 1, outcome: MediaOutcome::Finished });
        assert!(fx.is_empty(), "the superseded runner's late report is dropped");
        assert!(matches!(sim.s.active, Active::Media { epoch: 2, .. }));
    }

    // --- stop -------------------------------------------------------------

    #[test]
    fn stop_during_media_cancels_it_and_still_keeps_the_queue() {
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.queue.push(media_item("m2"));
        sim.push_spotify(1, "s1");
        let fx = sim.stop();
        assert!(has_cancel(&fx));
        assert!(has_leave(&fx));
        assert_eq!(sim.s.queue.len(), 2, "queued items outlive the stop");
        // The runner's cancel report lands after the bot has already left:
        // nothing starts, because the turn and the voice call are both gone.
        let fx = sim.media_ended(MediaOutcome::Cancelled);
        assert!(!has_start_media(&fx), "a stopped bot does not start the next item");
        assert!(matches!(sim.s.active, Active::None));
    }

    // --- explicit activation ----------------------------------------------

    #[test]
    fn enqueue_spotify_head_with_no_session_prompts() {
        // No link at all: nothing to claim through, so the prompt.
        let mut sim = Sim::new();
        let fx = sim.enqueue_start(spotify_item(1, "s1"));
        assert!(has_takeover_prompt(&fx));
        assert!(spircs(&fx).is_empty());
    }

    #[test]
    fn after_media_the_request_plays_before_the_cued_context_track() {
        // Live E: skipping S0 onto a media item cued the playlist's next
        // track paused at 0:00; resuming after the item played that track
        // before the queued request. With the request confirmed in
        // Spotify's queue, advance onto it instead.
        let mut sim = Sim::media_over_paused_baseline(); // cued: 9, paused BotForMedia
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Next, SpircCmd::Play]);
        assert!(matches!(&sim.s.active, Active::SpotifyPending { uri: u, .. } if *u == uri(1)));

        // Cued track IS the head (arm landed on it already): plain resume.
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.sp = SpDevice::Paused(uri(1));
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);

        // Arm not confirmed: resume and let the arm land at the boundary.
        let mut sim = Sim::media_over_paused_baseline();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Lost);
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
    }

    #[test]
    fn spotify_head_reaching_its_turn_claims_the_device() {
        // Live E'' on a fresh boot: the session came up without claiming
        // the device (F15) and a queued Spotify track then needed a manual
        // ▶. The request itself is the claim — enqueue-start, skip and the
        // post-media boundary all activate and load.
        let mut sim = Sim::new();
        sim.s.link_up = true;
        let fx = sim.enqueue_start(spotify_item(1, "s1"));
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice, SpircCmd::Load(uri(1))]);
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));

        let mut sim = Sim::media_over_paused_baseline();
        sim.s.link_up = true;
        sim.s.device_active = false;
        sim.s.sp = SpDevice::Inactive;
        sim.push_spotify(2, "s2");
        sim.skip();
        let fx = sim.media_ended(MediaOutcome::Cancelled);
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice, SpircCmd::Load(uri(2))]);
    }

    #[test]
    fn play_takeover_activates_the_device_explicitly() {
        // ▶ is the takeover gesture; nothing else ever claims the device.
        // A never-loaded session is idle once claimed, so a Spotify head
        // loads in the same press.
        let mut sim = Sim::new();
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice, SpircCmd::Load(uri(1))]);
        assert!(sim.s.device_active);
        // With nothing queued: activation alone.
        let mut sim = Sim::new();
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice]);
    }

    // --- controls during media --------------------------------------------

    #[test]
    fn previous_refuses_during_a_queue_item() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.previous();
        assert!(spircs(&fx).is_empty());
        assert_eq!(reply_text(&fx), "❌ Previous isn't available during queue playback.");
    }

    #[test]
    fn toggle_pause_during_media_uses_the_track_handle() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.toggle();
        assert!(fx.iter().any(|e| matches!(e, Effect::TrackHandle(TrackHandleCmd::Pause))));
        assert!(spircs(&fx).is_empty(), "the baseline is not touched");
        assert!(matches!(sim.s.active, Active::Media { paused: true, .. }));
        let fx = sim.toggle();
        assert!(fx.iter().any(|e| matches!(e, Effect::TrackHandle(TrackHandleCmd::Resume))));
        assert!(matches!(sim.s.active, Active::Media { paused: false, .. }));
    }

    // --- voice ------------------------------------------------------------

    #[test]
    fn voice_lost_during_media_cancels_it_and_stales_the_runner() {
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.step(Input::VoiceLost);
        assert!(has_cancel(&fx) && has_clear(&fx));
        assert!(matches!(sim.s.active, Active::None));
        let fx = sim.step(Input::MediaEnded { epoch: 1, outcome: MediaOutcome::Cancelled });
        assert!(fx.is_empty(), "the dead runner's report is stale");
    }

    #[test]
    fn a_load_without_voice_joins_first() {
        // The Spotify half of the same rule as the media case below. This
        // path had no test, and it is one of the two that shipped the live
        // 2026-09-01 silence — audio decoding into a bridge no call drains.
        let mut sim = Sim::idle_device();
        sim.s.voice = VoiceStatus::Down;
        let fx = sim.enqueue_start(spotify_item(1, "s1"));
        let join = fx.iter().position(|e| matches!(e, Effect::JoinVoice));
        let load = fx
            .iter()
            .position(|e| matches!(e, Effect::Spirc(SpircCmd::Load(_))));
        assert!(join.is_some() && load.is_some());
        assert!(join < load, "the join is requested before the load");
        assert_eq!(sim.s.voice, VoiceStatus::Joining);
    }

    #[test]
    fn a_bare_load_drops_the_playlist_it_replaces() {
        // A `Load` replaces the device's context, and Spotify reports a bare
        // track's context as absent — which never overwrites. Left standing,
        // the old playlist is stamped on this track's history row and a
        // later ⏮ reopens a playlist it was never in.
        let mut sim = Sim::idle_device();
        sim.s.context_uri = Some("spotify:playlist:old".into());
        sim.enqueue_start(spotify_item(1, "s1"));
        assert_eq!(sim.s.context_uri, None);
    }

    #[test]
    fn arming_is_refused_once_the_device_is_released() {
        // The F2 half of `maybe_arm`'s guard, on its own: a command sent
        // while this device is not the active one is silently dropped, so an
        // arm issued there is void rather than queued.
        //
        // The mirror is deliberately left reporting a live context, because
        // that is the only other thing that could refuse the arm. With it
        // saying `Playing`, the device check is the sole reason nothing goes
        // out — so deleting that check fails this test.
        let mut sim = Sim::baseline_playing();
        sim.s.device_active = false;
        assert!(matches!(sim.s.sp, SpDevice::Playing(_)));

        let fx = sim.enqueue(spotify_item(1, "s1"));

        assert!(spircs(&fx).is_empty(), "nothing is sent to a released device");
        assert!(sim.s.armed.is_none());
    }

    #[test]
    fn stop_releases_the_device_mirror_with_the_device() {
        let mut sim = Sim::baseline_playing();
        sim.stop();
        assert_eq!(
            sim.s.sp,
            SpDevice::Inactive,
            "the mirror must not claim a released device is playing"
        );
    }

    #[test]
    fn stop_lets_the_same_track_be_recorded_again() {
        // The card is gone after a stop, so the next airing is new even if
        // it is the same track. Without clearing this, the duplicate-card
        // guard also swallows that airing's history row.
        let mut sim = Sim::baseline_playing();
        sim.stop();
        assert_eq!(sim.s.last_heard_track, None);
    }

    #[test]
    fn stop_keeps_an_arm_it_sends_no_disconnect_for() {
        // Clearing the arm is justified only by `Disconnect` resetting
        // librespot's own queue. With the device already released no
        // `Disconnect` goes out, so the track really is still queued there —
        // and dropping the arm would let `maybe_arm` queue it a second time,
        // which librespot has no way to undo.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.s.device_active = false;

        let fx = sim.stop();
        assert!(
            !spircs(&fx).contains(&SpircCmd::Disconnect),
            "nothing to disconnect from"
        );
        assert!(sim.s.armed.is_some(), "so the arm is still real device state");
    }

    #[test]
    fn a_new_session_drops_the_previous_djs_walk() {
        // An account switch arrives as a bare `LinkUp` with no `LinkDown`
        // before it, so anything the previous session left in flight has to
        // be dropped here — a jump into a session that no longer exists can
        // never land, and a walk position points at another DJ's listening.
        let mut sim = Sim::baseline_playing();
        sim.s.history_cursor = Some(42);
        sim.s.awaiting_jump = Some(PendingJump::Context(uri(3)));
        sim.s.pending_request = Some(spotify_item(4, "held"));

        sim.step(Input::LinkUp { gen: sim.s.link_gen });

        assert_eq!(sim.s.history_cursor, None);
        assert_eq!(sim.s.awaiting_jump, None);
        assert!(sim.s.pending_request.is_none());
    }

    #[test]
    fn media_start_without_voice_joins_first() {
        let mut sim = Sim::idle_device();
        sim.s.voice = VoiceStatus::Down;
        sim.s.queue.push(media_item("m"));
        let fx = sim.toggle();
        let join = fx.iter().position(|e| matches!(e, Effect::JoinVoice));
        let start = fx.iter().position(|e| matches!(e, Effect::StartMedia { .. }));
        assert!(join.is_some() && start.is_some());
        assert!(join < start, "the join is requested before the start");
        assert_eq!(sim.s.voice, VoiceStatus::Joining);
    }

    // --- pending resolution -----------------------------------------------

    #[test]
    fn spotify_pending_wrong_uri_hands_the_turn_to_the_baseline() {
        // Something other than the loaded uri started: the turn passes to
        // the baseline and the head decision re-runs (the queued track
        // re-enters via arming, so it still airs).
        let mut sim = Sim::idle_device();
        sim.push_spotify(1, "s1");
        sim.toggle(); // Load(uri 1), pending
        let fx = sim.transport(TransportEvent::Playing { uri: uri(3), meta: None });
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
        assert_eq!(add_to_queue_count(&fx), 1, "the queued track re-arms");
        sim.advance(Duration::from_millis(5100));
        let fx = sim.step(Input::Tick(TimerKind::SpotifyPending));
        assert!(fx.is_empty(), "the resolved pending's timer is a no-op");
    }

    // --- misc guards ------------------------------------------------------

    #[test]
    fn enqueue_media_start_if_idle_starts_the_head() {
        let mut sim = Sim::idle_device();
        let fx = sim.enqueue_start(media_item("m"));
        assert!(has_start_media(&fx));
        assert_eq!(start_gate(&fx), Some(StartGate::Immediate));
        assert!(matches!(sim.s.active, Active::Media { .. }));
        assert_eq!(reply_text(&fx), "▶ Playing now");
    }

    #[test]
    fn enqueue_start_if_idle_never_steals_the_turn() {
        // With a turn holder the enqueue only queues (and arms) — the old
        // Enqueue trigger's whole contract.
        let mut sim = Sim::baseline_playing();
        let fx = sim.enqueue_start(media_item("m"));
        assert!(!has_start_media(&fx));
        assert_eq!(reply_text(&fx), "➕ Added to queue #1");
        assert!(without_reply(fx).iter().all(|e| !matches!(e, Effect::Spirc(_))));
        assert_eq!(sim.s.queue.len(), 1);
    }

    #[test]
    fn link_reconnecting_is_not_link_down() {
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.step(Input::LinkReconnecting { gen: 1 });
        // Only the bridge clear (Spotify held the turn) — no armed-clearing,
        // no turn change on a fast reconnect.
        assert!(matches!(fx.as_slice(), [Effect::ClearBridge]));
        assert!(sim.s.armed.is_some(), "no armed-clearing on a fast reconnect");
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
    }

    #[test]
    fn session_disconnect_voids_an_unacked_arm_but_keeps_a_confirmed_one() {
        // F2: an unacked add_to_queue was void — clear it so the next
        // opportunity re-arms. A Confirmed arm is real device-side state.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Sent(sim.now));
        sim.transport(TransportEvent::SessionDisconnected);
        assert!(sim.s.armed.is_none());
        assert!(!sim.s.device_active);

        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        sim.transport(TransportEvent::SessionDisconnected);
        assert!(sim.s.armed.is_some(), "clearing a Confirmed arm would double-queue on restore");
    }

    #[test]
    fn playing_same_track_again_does_not_repost_the_card() {
        // Seek/resume re-emits Playing for the same track; presence updates
        // but the card does not repost (the double-card/double-announce
        // guard).
        let mut sim = Sim::baseline_playing();
        sim.s.last_heard_track = None; // nothing carded yet
        let fx = sim.transport(TransportEvent::Playing { uri: uri(9), meta: Some(meta("t")) });
        assert!(has_spotify_card(&fx), "first Playing for a track posts the card");
        let fx = sim.transport(TransportEvent::Playing { uri: uri(9), meta: Some(meta("t")) });
        assert!(!has_spotify_card(&fx));
        assert!(has_presence(&fx));
    }

    // --- now playing (/np) -------------------------------------------------

    #[test]
    fn query_reports_nothing_playing_right_now() {
        let mut sim = Sim::new();
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        let snap = reply_snapshot(&fx);
        assert_eq!(snap.now, NowPlaying::Nothing);
        assert_eq!(snap.queue_len, 0);
        assert_eq!(snap.more, 0);
        assert!(snap.preview.is_empty());
    }

    #[test]
    fn query_reports_the_media_track_and_queued_by() {
        let mut sim = Sim::media_over_paused_baseline();
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        let snap = reply_snapshot(&fx);
        assert_eq!(
            snap.now,
            NowPlaying::Media {
                title: "active-item".into(),
                subtitle: "c".into(),
                queued_by: "dj".into(),
                paused: false,
            }
        );
    }

    #[test]
    fn query_reports_a_paused_media_track() {
        let mut sim = Sim::new();
        let item = media_item("paused-item");
        sim.s.active = Active::Media { item, paused: true, epoch: 0 };
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        let snap = reply_snapshot(&fx);
        assert_eq!(
            snap.now,
            NowPlaying::Media {
                title: "paused-item".into(),
                subtitle: "c".into(),
                queued_by: "dj".into(),
                paused: true,
            }
        );
    }

    #[test]
    fn query_reports_the_spotify_track() {
        let mut sim = Sim::baseline_playing();
        sim.s.active = Active::Spotify { track: Some(meta("t")) };
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        let snap = reply_snapshot(&fx);
        assert_eq!(
            snap.now,
            NowPlaying::Spotify { title: "t".into(), artist: "artist".into(), paused: false }
        );
    }

    #[test]
    fn query_maps_spotify_pending_to_starting() {
        let mut sim = Sim::idle_device();
        sim.push_spotify(1, "s1");
        sim.toggle(); // begin_load -> SpotifyPending
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        assert_eq!(reply_snapshot(&fx).now, NowPlaying::SpotifyStarting);
    }

    #[test]
    fn query_preview_caps_at_five_and_reports_the_remainder() {
        let mut sim = Sim::new();
        for i in 0..8 {
            assert!(sim.s.queue.push(media_item(&format!("t{i}"))));
        }
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        let snap = reply_snapshot(&fx);
        assert_eq!(snap.queue_len, 8);
        assert_eq!(snap.preview.len(), 5);
        assert_eq!(snap.more, 3);
        assert_eq!(snap.preview[0].title, "t0");
        assert_eq!(snap.preview[4].title, "t4");
    }

    #[test]
    fn query_flags_the_armed_entry_in_the_preview() {
        let mut sim = Sim::baseline_playing();
        let id1 = sim.push_spotify(1, "s1");
        let id2 = sim.push_spotify(2, "s2");
        sim.arm(2, id2, Ack::Confirmed);
        let (tx, _rx) = oneshot::channel();
        let fx = sim.step(Input::Query { reply: tx });
        let snap = reply_snapshot(&fx);
        let armed: Vec<u64> = snap.preview.iter().filter(|e| e.armed).map(|e| e.item_id).collect();
        assert_eq!(armed, vec![id2]);
        assert!(!snap.preview.iter().find(|e| e.item_id == id1).unwrap().armed);
    }
}
