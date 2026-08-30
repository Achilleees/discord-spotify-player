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
    Media { item_id: u64, item: QueueItem, paused: bool, epoch: u64 },
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
    BotForStop,
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
/// `Idle` is reachable only from `Stopped` while `device_active`; a takeover
/// emits `SessionDisconnected` then `Stopped`, which lands in `Inactive` and
/// must never be read as "safe to `load()` over".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpDevice {
    Inactive,
    Idle,
    /// `EndOfTrack` seen; librespot's auto-advance is imminent.
    Boundary,
    Playing(SpotifyUri),
    Paused(SpotifyUri),
}

/// Mirror of the Discord voice connection. Media starts while not `Ready`
/// emit `JoinVoice` alongside `StartMedia`; the runner blocks on the voice
/// handle, so the core needs no deferred-start bookkeeping.
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
    armed_snapshot: Option<ArmSnapshot>,
}

impl PlayerState {
    pub fn new() -> Self {
        Self {
            queue: PriorityQueue::new(),
            active: Active::None,
            sp: SpDevice::Inactive,
            link_up: false,
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
    SetQueue { current: Option<SpotifyUri>, queued: Vec<SpotifyUri> },
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
    Stop { reply: oneshot::Sender<String> },
    TogglePause { reply: oneshot::Sender<String> },
    Previous { reply: oneshot::Sender<String> },
    /// A media runner's terminal report, epoch-tagged against stale runners.
    MediaEnded { epoch: u64, outcome: MediaOutcome },
    Transport { gen: u64, ev: TransportEvent },
    LinkUp { gen: u64 },
    LinkDown { gen: u64 },
    /// Fast reconnect in progress — informational, never an armed-clearing
    /// event.
    LinkReconnecting { gen: u64 },
    VoiceReady,
    VoiceLost,
    /// An explicit human claim on the Connect device (`/login`): the one
    /// path besides ▶ that may activate. Auto-start and on-demand sessions
    /// never send it (F15).
    ActivateDevice,
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
    LeaveVoice,
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
                        if !state.device_active {
                            // Explicit activation only: never Load/activate
                            // over the DJ's phone from an enqueue.
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
                    if !state.device_active {
                        fx.push(Effect::Ui(UiMsg::TakeoverPrompt));
                        reply(
                            &mut fx,
                            tx,
                            "⚠️ Spotify is in use on another device — press ▶ to take over.",
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

        Input::Stop { reply: tx } => {
            // Stop on a radio means dead air: clear the queue, silence
            // everything, and let ⏯ be the thing that resumes.
            let orphaned = state.armed.take().is_some();
            state.armed_snapshot = None;
            state.queue.clear();
            let mut paused_now = false;
            if matches!(state.active, Active::Media { .. }) {
                fx.push(Effect::CancelMedia);
                fx.push(Effect::ClearBridge);
                // `active` stays Media until the runner reports
                // `MediaEnded{Cancelled}`; the emptied queue plus
                // `BotForStop` below make that boundary land on dead air.
            } else if matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. })
            {
                if state.device_active && matches!(state.sp, SpDevice::Playing(_)) {
                    fx.push(Effect::Spirc(SpircCmd::Pause));
                    state.inflight.record_pause(now);
                    paused_now = true;
                }
                fx.push(Effect::ClearBridge);
                state.active = Active::None;
                fx.push(Effect::Presence(PresenceState::Idle));
            }
            if paused_now || matches!(state.pause_owner, Some(PauseOwner::BotForMedia)) {
                state.pause_owner = Some(PauseOwner::BotForStop);
            }
            let text = if orphaned {
                // Librespot has no dequeue, so an armed track can't be
                // un-armed — it may still air once when the baseline plays.
                "⏹ Stopped. Queue cleared. (a track already handed to Spotify will still play once)"
            } else {
                "⏹ Stopped. Queue cleared."
            };
            reply(&mut fx, tx, text);
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
                if matches!(head_of(&state.queue), Head::Media) {
                    let item = state.queue.pop().expect("head checked as media");
                    let title = item.source.display_title().to_string();
                    start_media(state, item, StartGate::Immediate, &mut fx);
                    reply(&mut fx, tx, format!("▶ Taking over the Spotify device — starting **{title}**."));
                } else {
                    reply(&mut fx, tx, "▶ Taking over the Spotify device.");
                }
                return fx;
            }
            match head_of(&state.queue) {
                Head::Media => {
                    // Old table: "play_button_drains_a_media_head" — the
                    // head airs; a pre-existing pause keeps its owner, so a
                    // phone pause still blocks the auto-resume afterwards.
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
                        fx.push(Effect::Spirc(SpircCmd::Play));
                        state.pause_owner = None;
                        state.active = Active::Spotify { track: None };
                        reply(&mut fx, tx, "▶ Resumed");
                    }
                }
                Head::Empty => {
                    if matches!(state.sp, SpDevice::Paused(_) | SpDevice::Boundary) {
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
                fx.push(Effect::Spirc(SpircCmd::Previous));
                reply(&mut fx, tx, "⏮ Previous track.");
            } else {
                reply(&mut fx, tx, "Nothing is playing right now.");
            }
        }

        Input::MediaEnded { epoch, outcome: _ } => {
            // A stale runner (superseded epoch) or an already-resolved turn
            // has nothing to say.
            if epoch != state.media_epoch || !matches!(state.active, Active::Media { .. }) {
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

        Input::ActivateDevice => {
            if !state.device_active {
                fx.push(Effect::Spirc(SpircCmd::ActivateDevice));
                state.device_active = true;
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
            // Bookkeeping first: a matching request is consumed wherever it
            // sits (a track the DJ's playlist reaches isn't also aired
            // later), and the armed marker clears only for the armed uri.
            let popped = state.queue.remove_first(
                |i| matches!(&i.source, MediaSource::Spotify { uri: u, .. } if u == &uri),
            );
            if state.armed.as_ref().is_some_and(|a| a.uri == uri) {
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
                // pause lands.
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
            if !same_track {
                fx.push(Effect::Ui(UiMsg::NowPlayingSpotify { uri: uri.clone(), meta: meta.clone() }));
                if let Some(m) = &meta {
                    fx.push(Effect::Announce(AnnounceKind::Track {
                        title: m.title.clone(),
                        artist: m.artist.clone(),
                    }));
                }
                state.last_heard_track = Some(uri_str);
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
            // Idle is reachable only from Stopped while `device_active`; a
            // takeover emits SessionDisconnected then Stopped, which must
            // not read as "safe to load() over".
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
            if matches!(state.active, Active::Spotify { .. } | Active::SpotifyPending { .. }) {
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
                fx.push(Effect::Ui(UiMsg::NowPlayingSpotify { uri, meta: Some(meta) }));
                state.last_heard_track = Some(uri_str);
            }
        }

        TransportEvent::SetQueue { current, queued } => {
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

        TransportEvent::SessionConnected => {
            // Mirror only — `LinkUp` (from the session supervisor) is the
            // canonical reconnect signal and carries the new generation.
        }

        TransportEvent::SessionDisconnected => {
            state.device_active = false;
            state.sp = SpDevice::Inactive;
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
            if !state.device_active {
                // Nobody to talk to (link down or another device owns
                // playback). Keep `pause_owner`: LinkUp reconciliation
                // pays the resume debt when the session returns.
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
                    // resumes it; the armed head airs from that pause (or
                    // at the next boundary).
                    fx.push(Effect::Spirc(SpircCmd::Play));
                    state.pause_owner = None;
                    state.active = Active::Spotify { track: None };
                }
                Some(PauseOwner::Human) | Some(PauseOwner::BotForStop) => {
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
                // Human/stop pauses stay honoured; a BotForMedia debt while
                // the device is unreachable stays recorded for LinkUp.
                state.active = Active::None;
                fx.push(Effect::Presence(PresenceState::Idle));
            }
        }
    }
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
    state.pause_owner = None;
    state.active = Active::SpotifyPending { uri: uri.clone(), sent: now, retried: false };
    fx.push(Effect::Spirc(SpircCmd::Load(uri)));
    fx.push(Effect::SetTimer(TimerKind::SpotifyPending, PENDING_TIMEOUT));
}

/// Hand the turn to a media item: bump the epoch, make sure voice is coming
/// up, and emit the gated start plus its card.
fn start_media(state: &mut PlayerState, item: QueueItem, gate: StartGate, fx: &mut Vec<Effect>) {
    state.media_epoch += 1;
    if !matches!(state.voice, VoiceStatus::Ready) {
        if matches!(state.voice, VoiceStatus::Down) {
            fx.push(Effect::JoinVoice);
        }
        state.voice = VoiceStatus::Joining;
    }
    fx.push(Effect::StartMedia { item: item.clone(), epoch: state.media_epoch, gate });
    fx.push(Effect::Ui(UiMsg::NowPlayingMedia { item: item.clone() }));
    state.active = Active::Media {
        item_id: item.item_id,
        item,
        paused: false,
        epoch: state.media_epoch,
    };
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
                Active::Media { item_id: item.item_id, item, paused: false, epoch: 1 };
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
            self.step(Input::Stop { reply: tx })
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
        // Old: enqueue_arms_while_spotify_is_playing_regardless_of_head —
        // arming looks past the media head for the first Spotify item.
        let mut sim = Sim::baseline_playing();
        sim.s.queue.push(media_item("m1"));
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
        assert!(timer_set(&fx, TimerKind::ArmAck));
        assert!(matches!(sim.s.armed, Some(Armed { ack: Ack::Sent(_), .. })));
    }

    #[test]
    fn enqueue_never_double_arms() {
        // Old: the armed-head rows of enqueue_arms_* — one armed track at a
        // time; a second enqueue changes nothing device-side.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.enqueue(spotify_item(2, "s2"));
        assert_eq!(add_to_queue_count(&fx), 0, "one armed track at a time");
    }

    #[test]
    fn enqueue_during_media_arms_but_never_disturbs_the_media() {
        // Deliberate change from the old table's
        // enqueue_never_arms_while_media_is_active: arming while the
        // baseline sits bot-paused under a media item is what sets up the
        // paused-at-0:00 handoff (and the frozen-skip fix). The invariant
        // that survives is the second half: an enqueue during media never
        // starts or silences anything.
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.enqueue(spotify_item(1, "s1"));
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
        assert!(!has_start_media(&fx) && !has_cancel(&fx) && !has_clear(&fx));
        assert!(matches!(sim.s.active, Active::Media { .. }));
    }

    #[test]
    fn enqueue_does_not_arm_while_spotify_is_idle() {
        // Old: enqueue_does_nothing_unless_spotify_is_playing (idle half) —
        // there is no context to queue behind.
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
        // Deliberate change from the old table's paused half of
        // enqueue_does_nothing_unless_spotify_is_playing: a paused context
        // is still a context, and arming into it is what makes a later
        // skip/resume air the request (frozen-skip setup).
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
        // Old: track_end_leaves_an_armed_spotify_head_alone — auto-advance
        // lands on the armed track by itself.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(spircs(&fx).is_empty());
        assert_eq!(sim.s.sp, SpDevice::Boundary);
    }

    #[test]
    fn end_of_track_arms_an_unarmed_spotify_head() {
        // Old: track_end_queues_behind_current_for_an_unarmed_spotify_head.
        let mut sim = Sim::baseline_playing();
        sim.push_spotify(1, "s1");
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1))]);
    }

    #[test]
    fn end_of_track_with_media_head_starts_it_behind_the_pause_ack() {
        // Old: track_end_drains_a_media_head. No Pause at the boundary —
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
        // Old: track_end_does_nothing_while_media_is_active_racing_a_drain.
        let mut sim = Sim::media_over_paused_baseline();
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(fx.is_empty());
        assert!(matches!(sim.s.active, Active::Media { .. }));
    }

    #[test]
    fn end_of_track_with_empty_queue_lets_the_baseline_roll() {
        // Old: track_end_does_nothing_on_an_empty_queue.
        let mut sim = Sim::baseline_playing();
        let fx = sim.transport(TransportEvent::EndOfTrack);
        assert!(fx.is_empty());
    }

    // --- media-end boundaries (old media_end_* rows) ----------------------

    #[test]
    fn media_end_resumes_an_armed_spotify_head() {
        // Old: media_end_resumes_an_armed_spotify_head.
        let mut sim = Sim::media_over_paused_baseline();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
        assert_eq!(sim.s.pause_owner, None);
    }

    #[test]
    fn media_end_arms_and_resumes_an_unarmed_spotify_head() {
        // Old: media_end_queues_behind_current_for_an_unarmed_spotify_head
        // (QueueThenResume): queue it behind the context, then resume.
        let mut sim = Sim::media_over_paused_baseline();
        sim.push_spotify(1, "s1");
        let fx = sim.media_ended(MediaOutcome::Finished);
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1)), SpircCmd::Play]);
    }

    #[test]
    fn media_end_loads_a_spotify_head_while_idle() {
        // Old: media_end_loads_an_unarmed_spotify_head_while_idle — no
        // context to lose, so load() is allowed.
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
        // Old: media_end_does_nothing_on_an_empty_queue — the resume lived
        // in the drain loop's `resume_spotify_after_drain` bool then; the
        // pause owner carries that decision now.
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
        // Old: skip_jumps_to_an_armed_spotify_head.
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
        // Old: skip_queues_behind_current_then_skips_an_unarmed_spotify_head.
        let mut sim = Sim::baseline_playing();
        sim.push_spotify(1, "s1");
        let fx = sim.skip();
        let cmds = spircs(&fx);
        assert_eq!(cmds[0], SpircCmd::AddToQueue(uri(1)), "queue behind current first");
        assert!(cmds.contains(&SpircCmd::Next));
    }

    #[test]
    fn skip_onto_a_media_head_pauses_advances_and_starts_it() {
        // Old: skip_sends_next_then_drains_a_media_head, upgraded to F4's
        // pause(); next(): the silent advance loads the skipped track
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
        // Old: skip_on_an_empty_queue_skips_the_spotify_baseline.
        let mut sim = Sim::baseline_playing();
        let fx = sim.skip();
        assert_eq!(spircs(&fx), vec![SpircCmd::Next]);
    }

    // --- the play button (old play_button_* rows) -------------------------

    #[test]
    fn play_when_paused_with_armed_head_resumes() {
        // Old: play_button_resumes_an_armed_spotify_head.
        let mut sim = Sim::baseline_paused(None);
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::Play]);
        assert!(matches!(sim.s.active, Active::Spotify { .. }));
    }

    #[test]
    fn play_when_idle_loads_the_spotify_head() {
        // Old: play_button_loads_an_unarmed_spotify_head_while_idle.
        let mut sim = Sim::idle_device();
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::Load(uri(1))]);
        assert!(matches!(sim.s.active, Active::SpotifyPending { .. }));
        assert!(timer_set(&fx, TimerKind::SpotifyPending));
    }

    #[test]
    fn play_with_a_paused_baseline_arms_and_resumes_never_loads() {
        // Old: play_button_never_hijacks_a_paused_baseline — a paused
        // context is the DJ's; queue behind it and resume, never Load.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::AddToQueue(uri(1)), SpircCmd::Play]);
    }

    #[test]
    fn play_with_a_media_head_starts_it_and_leaves_the_baseline_alone() {
        // Old: play_button_drains_a_media_head — the baseline stays paused
        // and keeps its owner.
        let mut sim = Sim::baseline_paused(Some(PauseOwner::Human));
        sim.s.queue.push(media_item("m"));
        let fx = sim.toggle();
        assert!(spircs(&fx).is_empty());
        assert!(has_start_media(&fx));
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::Human));
    }

    #[test]
    fn play_with_empty_queue_resumes_the_baseline() {
        // Old: play_button_on_an_empty_queue_resumes_the_baseline. An
        // explicit Discord command overrides even a human phone pause.
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
    fn stop_orphans_armed_track() {
        // Librespot has no dequeue: stop clears our side and says so.
        let mut sim = Sim::baseline_playing();
        let id = sim.push_spotify(1, "s1");
        sim.arm(1, id, Ack::Confirmed);
        let fx = sim.stop();
        assert!(sim.s.queue.is_empty());
        assert!(sim.s.armed.is_none());
        assert_eq!(
            reply_text(&fx),
            "⏹ Stopped. Queue cleared. (a track already handed to Spotify will still play once)"
        );
        assert_eq!(spircs(&fx), vec![SpircCmd::Pause]);
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForStop));
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

    // --- SetQueue ack machine ---------------------------------------------

    #[test]
    fn set_queue_confirms_a_sent_arm() {
        let mut sim = Sim::baseline_playing();
        sim.enqueue(spotify_item(1, "s1")); // arms, Sent
        let fx = sim.transport(TransportEvent::SetQueue {
            current: Some(uri(9)),
            queued: vec![uri(1)],
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
        let fx = sim.transport(TransportEvent::SetQueue { current: Some(uri(9)), queued: vec![] });
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
        let fx = sim.transport(TransportEvent::SetQueue { current: Some(uri(1)), queued: vec![] });
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
    fn stop_during_media_goes_to_dead_air() {
        let mut sim = Sim::media_over_paused_baseline();
        sim.s.queue.push(media_item("m2"));
        sim.push_spotify(1, "s1");
        let fx = sim.stop();
        assert!(has_cancel(&fx));
        assert!(sim.s.queue.is_empty());
        assert_eq!(sim.s.pause_owner, Some(PauseOwner::BotForStop));
        // The runner's cancel report lands on an empty queue and a stop-
        // owned pause: dead air, no resume, no next item.
        let fx = sim.media_ended(MediaOutcome::Cancelled);
        assert!(spircs(&fx).is_empty() && !has_start_media(&fx), "stop means dead air");
        assert!(matches!(sim.s.active, Active::None));
    }

    // --- explicit activation ----------------------------------------------

    #[test]
    fn enqueue_spotify_head_while_inactive_prompts_takeover() {
        // A Spotify head while another device is active blocks with the
        // takeover prompt — never a blind Load or activate.
        let mut sim = Sim::new();
        let fx = sim.enqueue_start(spotify_item(1, "s1"));
        assert!(has_takeover_prompt(&fx));
        assert!(spircs(&fx).is_empty());
    }

    #[test]
    fn play_takeover_activates_the_device_explicitly() {
        // ▶ is the takeover gesture; nothing else ever claims the device.
        let mut sim = Sim::new();
        sim.push_spotify(1, "s1");
        let fx = sim.toggle();
        assert_eq!(spircs(&fx), vec![SpircCmd::ActivateDevice]);
        assert!(sim.s.device_active);
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
        sim.s.active = Active::Media { item_id: item.item_id, item, paused: true, epoch: 0 };
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
