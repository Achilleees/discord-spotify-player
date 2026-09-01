//! Player actor: the impure shell around the pure core in [`super::state`].
//!
//! One task owns a [`PlayerState`] and a mailbox of [`Input`]s; everything
//! else talks to it through a [`PlayerHandle`]. Each input runs one
//! `step(state, input, now)` and then the returned effects, in order.
//! **The actor awaits nothing, ever**: every effect is a synchronous channel
//! send, an atomic store, a `CancellationToken::cancel`, or a `tokio::spawn`
//! (media runners, voice joins, announcements, timers), so a step can never
//! park the mailbox behind IO. Asynchronous completions come back as inputs
//! — `MediaEnded` tagged with its epoch, `VoiceReady`/`VoiceLost` from the
//! join task, `Tick` from spawned timers — so stale reports are ignored by
//! the core, not raced by the shell.
//!
//! The actor is the single owner of playback state: the queue, the armed
//! track and the turn all live in its [`PlayerState`], and every playback
//! surface — Spirc commands, the now-playing cards, DJ announcements, the
//! bot's status line, the shared bridge-reader track — is driven from here.
//! The `spirc_cmd_tx` cell is shared with the session supervisor, which
//! (re)publishes the live session's sender on switch/stop; the actor is its
//! only playback-command sender, while [`PlayerHandle::lookup_spotify`]
//! borrows it for metadata lookups that run in the *caller's* task, never
//! in this one.
//!
//! Two things are shell state rather than core state, because the core
//! deliberately doesn't model them: the bridge-reader `TrackHandle` follows
//! raw transport telemetry (Spotify playing resumes it, Spotify going quiet
//! pauses it — unless a media item holds the turn and needs it live), and
//! the status line's view of a running media item (`PresenceState` is
//! Spotify-only, so `StartMedia`/`TrackHandle` effects feed the media
//! title/pause state to the presence loop directly).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use songbird::tracks::TrackHandle;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;

use crate::audio::dj::DJAnnouncer;
use crate::audio_bridge::AudioBridge;
use crate::player::state::{
    step, Active, AnnounceKind, Effect, EnqueuePos, Input, MediaOutcome, NowPlaying,
    PlayerSnapshot, PlayerState, PresenceState, SpDevice, SpircCmd, StartGate, TrackHandleCmd,
    TrackMeta, TransportEvent, UiMsg as CoreUiMsg,
};
use crate::presence::PresenceUpdate;
use crate::queue::{MediaSource, QueueItem};
use crate::spotify::SpircCommand;
use crate::youtube::feeder::{feed_file_to_bridge, feed_youtube_to_bridge, FeederError};
use librespot_core::SpotifyUri;

/// Reply used when the actor task is gone — unreachable in practice, since
/// the Handler holds a `PlayerHandle` for the life of the process.
const NO_ACTOR_REPLY: &str = "⚠️ The player didn't respond — try again.";

/// How long a gated media runner waits for the Spotify pause ack before
/// starting anyway (mirrors the old fixed post-`Pause` sleep).
const PAUSE_ACK_FALLBACK_MS: u64 = 500;

/// Discord-side UI requests the actor and its media runners emit; the bot
/// layer maps them onto the UI task's own message type (which is private to
/// the `discord` module) and resolves the task's mailbox per send.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A queue (YouTube/file) item took the turn — post its card.
    NowPlayingMedia { item: QueueItem },
    /// The Spotify baseline took the turn on a new track — post its card.
    NowPlayingSpotify { uri: SpotifyUri, meta: Option<TrackMeta> },
    /// A queue item finished naturally — post its history embed.
    HistoryMedia { item: QueueItem },
    /// Delete the current card and post the idle controls card.
    IdleCard,
    /// Repaint the card's pause/resume button.
    Buttons { paused: bool },
}

/// Synchronous UI dispatch built by the bot layer (a closure over the UI
/// task's mailbox slot). Must never block.
pub type UiSendFn = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// Voice-join dispatch built by the bot layer: ensures the bot is in a
/// voice call (following the given user when it has to join fresh) and
/// resolves `true` once the call exists. The actor only ever runs it inside
/// a spawned task.
pub type JoinVoiceFn =
    Arc<dyn Fn(Option<u64>) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Leaves the voice call. Deliberate departures only (`/stop`) — an empty
/// channel is torn down by the Discord layer. Run inside a spawned task.
pub type LeaveVoiceFn = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Everything the actor owns or drives. The shared `Arc`s are the process's
/// cross-task seams: the spirc cell is written by the session supervisor,
/// the track handle by the voice-join machinery, and `announce_enabled` by
/// `/announce`; the rest is the actor's own equipment.
pub struct PlayerDeps {
    pub bridge: Arc<AudioBridge>,
    pub ui_send: UiSendFn,
    /// Plain text-channel notices (failure messages, takeover prompts); a
    /// bot-layer task does the actual Discord send.
    pub notice_tx: mpsc::UnboundedSender<String>,
    /// Feeds the Discord status task (`run_presence_loop`).
    pub presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
    pub join_voice: JoinVoiceFn,
    pub leave_voice: LeaveVoiceFn,
    /// The live session's command channel; `None` between sessions. The
    /// session supervisor (re)publishes the sender on switch/stop.
    pub spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    /// The shared bridge-reader track, written by the voice-join machinery
    /// whenever the bot (re)joins a call.
    pub track_handle: Arc<Mutex<Option<TrackHandle>>>,
    pub dj: Arc<DJAnnouncer>,
    pub announce_enabled: Arc<AtomicBool>,
    /// Append-only log of what aired. `None` disables recording (tests, and
    /// a bot whose database could not be opened) without changing any
    /// playback behaviour.
    pub history: Option<Arc<crate::history::HistoryStore>>,
    /// Persists the pending queue across restarts. `None` disables it
    /// without changing playback.
    pub queue_store: Option<Arc<crate::queue_store::QueueStore>>,
}

/// The actor's mailbox handle. Cheap to clone; the typed helpers build the
/// `Input`, send it, and await the oneshot reply.
#[derive(Clone)]
pub struct PlayerHandle {
    tx: mpsc::UnboundedSender<Input>,
    /// The same spirc cell the actor holds, so `lookup_spotify` runs in the
    /// caller's task without a
    /// mailbox round-trip — and without the caller ever holding a channel it
    /// could drive playback with directly.
    spirc: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
}

impl PlayerHandle {
    /// Send a reply-less input (transport events, media/voice reports,
    /// timer ticks). Dropped silently if the actor is gone.
    pub fn send(&self, input: Input) {
        let _ = self.tx.send(input);
    }

    async fn request(&self, make: impl FnOnce(oneshot::Sender<String>) -> Input) -> String {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(make(tx)).is_err() {
            return NO_ACTOR_REPLY.to_string();
        }
        rx.await.unwrap_or_else(|_| NO_ACTOR_REPLY.to_string())
    }

    /// Queue an item; `start_if_idle` starts the head right away when
    /// nothing holds the turn (the `/play` semantics; `/queue` passes
    /// `false`).
    pub async fn enqueue(&self, item: QueueItem, pos: EnqueuePos, start_if_idle: bool) -> String {
        self.request(|reply| Input::Enqueue { item, pos, start_if_idle, reply }).await
    }

    pub async fn skip(&self) -> String {
        self.request(|reply| Input::Skip { reply }).await
    }

    pub async fn stop(&self) -> String {
        self.request(|reply| Input::Stop { reply }).await
    }

    pub async fn toggle_pause(&self) -> String {
        self.request(|reply| Input::TogglePause { reply }).await
    }

    /// Empty the queue, leaving whatever is audible alone.
    pub async fn clear_queue(&self) -> String {
        self.request(|reply| Input::ClearQueue { reply }).await
    }

    /// Hand the actor the queue the last process was holding. Fire-and-
    /// forget: there is no reply to wait for and nothing starts playing.
    pub fn restore_queue(&self, items: Vec<crate::queue::QueueItem>) {
        let _ = self.tx.send(Input::RestoreQueue { items });
    }

    /// The ▶ half only (bare `/play`): refused while something is audible.
    pub async fn play(&self) -> String {
        self.request(|reply| Input::Play { reply }).await
    }

    pub async fn previous(&self) -> String {
        self.request(|reply| Input::Previous { reply }).await
    }

    /// The player's structured view of itself — `/np` and the queue listing
    /// render from this. Falls back to an empty snapshot if the actor is
    /// gone (unreachable in practice, as for `request`).
    pub async fn query(&self) -> PlayerSnapshot {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Query { reply: tx }).is_err() {
            return empty_snapshot();
        }
        rx.await.unwrap_or_else(|_| empty_snapshot())
    }

    /// Resolves a Spotify track's title/artist/art through the live session
    /// (`SpircCommand::Lookup`), returning `None` if there is no session or
    /// the lookup itself fails — both cases the caller reports identically.
    /// Awaits in the caller's task, never inside the actor.
    pub async fn lookup_spotify(&self, uri: &SpotifyUri) -> Option<(String, String, Option<String>)> {
        let tx = { self.spirc.lock().clone() }?;
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx.send(SpircCommand::Lookup(uri.clone(), reply_tx)).is_err() {
            return None;
        }
        match reply_rx.await {
            Ok(Some(lookup)) => Some((lookup.title, lookup.artist, lookup.album_art_url)),
            Ok(None) | Err(_) => None,
        }
    }
}

fn empty_snapshot() -> PlayerSnapshot {
    PlayerSnapshot {
        now: NowPlaying::Nothing,
        queue_len: 0,
        preview: Vec::new(),
        more: 0,
        device_active: false,
        link_up: false,
    }
}

/// Spawn the player actor and return its handle. Called once, when the bot
/// is built; the actor lives for the process (lifecycle A).
pub fn spawn(deps: PlayerDeps) -> PlayerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = PlayerHandle { tx: tx.clone(), spirc: deps.spirc_cmd_tx.clone() };
    let actor = Actor {
        deps,
        tx,
        state: PlayerState::new(),
        pending_gate: None,
        feeder_cancel: None,
        feeder_paused: Arc::new(AtomicBool::new(false)),
        media_status: None,
        buttons_paused: false,
    };
    tokio::spawn(actor.run(rx));
    handle
}

struct Actor {
    deps: PlayerDeps,
    /// Own mailbox sender, cloned into runners/timers/join tasks so their
    /// completions come back as inputs.
    tx: mpsc::UnboundedSender<Input>,
    state: PlayerState,
    /// The gate of the most recent `AfterSpotifyPauseAck` start, fired on
    /// the next `Transport(Paused)` (the runner has a fallback timeout, so
    /// a swallowed ack can't wedge it).
    pending_gate: Option<Arc<Notify>>,
    /// Cancel token of the live media runner, registered before the runner
    /// spawns so a `CancelMedia` landing during its gate wait still reaches
    /// it.
    feeder_cancel: Option<CancellationToken>,
    /// Pause flag the live feeder polls, so a paused media item stops
    /// downloading ahead of what the bridge can hold.
    feeder_paused: Arc<AtomicBool>,
    /// Title/artist of the running media item as last pushed to the status
    /// line. The core's `PresenceState` is Spotify-only, so the shell feeds
    /// media status itself; a core presence effect supersedes this.
    media_status: Option<(String, String)>,
    /// Last pause state painted onto the card's ⏯ button, so telemetry
    /// echoes don't repaint an unchanged card.
    buttons_paused: bool,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>) {
        while let Some(input) = rx.recv().await {
            // Shell-side handling that reads raw inputs rather than core
            // decisions, run on receipt before the step.
            match &input {
                Input::Transport { ev, .. } => {
                    if let TransportEvent::Paused { .. } = ev {
                        if let Some(gate) = self.pending_gate.take() {
                            gate.notify_one();
                        }
                    }
                    self.drive_reader(ev);
                }
                Input::LinkUp { .. } => {
                    // An account switch never emits LinkDown for the session
                    // it replaces (see `spotify::session`): the old device
                    // queue — and any armed track on it — died with that
                    // session, so an arm still set at LinkUp is a ghost, not
                    // a live device-side queue entry. Reconnect restores go
                    // through `armed_snapshot`, which LinkDown fills, so
                    // clearing `armed` here can't touch them.
                    self.state.armed = None;
                }
                _ => {}
            }

            tracing::debug!(target: "player", ?input, "input");
            let queue_rev = self.state.queue.revision();
            let effects = step(&mut self.state, input, Instant::now());
            tracing::debug!(
                target: "player",
                active = ?self.state.active,
                sp = ?self.state.sp,
                armed = ?self.state.armed,
                device_active = self.state.device_active,
                queue_len = self.state.queue.len(),
                effects = ?effects,
                "step"
            );

            // The sink's turn gate: Spotify samples reach the bridge only
            // when no media item holds the turn.
            self.deps
                .bridge
                .set_spotify_muted(matches!(self.state.active, Active::Media { .. }));

            // Persist only when the queue actually changed — the actor sees
            // far more transport events than queue mutations, and a write
            // per event would be noise.
            if self.state.queue.revision() != queue_rev {
                if let Some(store) = self.deps.queue_store.clone() {
                    let items = self.state.queue.snapshot();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = store.save(&items) {
                            tracing::warn!(error = %e, "failed to persist the queue");
                        }
                    });
                }
            }

            // A `StartMedia` in this batch: remember whose item it is, so a
            // cold-start voice join follows the requester.
            let mut join_hint = None;
            for effect in &effects {
                if let Effect::StartMedia { item, .. } = effect {
                    join_hint = Some(item.queued_by_id);
                }
            }

            for effect in effects {
                self.run_effect(effect, join_hint);
            }

            // The media turn can end without a core presence effect (an
            // honoured human pause leaves dead air; a voice loss cancels the
            // item): reflect it on the status line. When the baseline is
            // taking over instead, its own `Playing` repaints the status,
            // so no Idle blip is needed.
            if self.media_status.is_some() && !matches!(self.state.active, Active::Media { .. }) {
                self.media_status = None;
                if matches!(self.state.active, Active::None) {
                    let _ = self.deps.presence_tx.send(PresenceUpdate::Idle);
                }
            }
        }
    }

    /// Drive the shared bridge-reader track from raw Spotify telemetry: a
    /// playing device is pushing audio, so the reader must run; a quiet one
    /// freezes the buffered tail in place — unless a media item holds the
    /// turn and needs the reader live. Also mirrors the baseline's pause
    /// state onto the card's ⏯ button (only while the baseline owns the
    /// card — a pause echo under a media item must not repaint its button).
    fn drive_reader(&mut self, ev: &TransportEvent) {
        let media_turn = matches!(self.state.active, Active::Media { .. });
        match ev {
            TransportEvent::Playing { .. } => {
                // A `Playing` under a user-paused media item is an
                // interloper the core pauses right back; resuming the
                // reader for it would audibly unfreeze the media item.
                if !matches!(self.state.active, Active::Media { paused: true, .. }) {
                    let handle = { self.deps.track_handle.lock().clone() };
                    if let Some(handle) = handle {
                        let _ = handle.play();
                    }
                }
                if !media_turn && self.buttons_paused {
                    self.buttons_paused = false;
                    (self.deps.ui_send)(UiEvent::Buttons { paused: false });
                }
            }
            TransportEvent::Paused { .. }
            | TransportEvent::Stopped
            | TransportEvent::EndOfTrack
            | TransportEvent::Unavailable { .. } => {
                if media_turn {
                    return;
                }
                let handle = { self.deps.track_handle.lock().clone() };
                if let Some(handle) = handle {
                    let _ = handle.pause();
                }
                if matches!(ev, TransportEvent::Paused { .. }) && !self.buttons_paused {
                    self.buttons_paused = true;
                    (self.deps.ui_send)(UiEvent::Buttons { paused: true });
                }
            }
            _ => {}
        }
    }

    /// Run one effect. Synchronous by contract: sends, stores, cancels and
    /// spawns only.
    fn run_effect(&mut self, effect: Effect, join_hint: Option<u64>) {
        match effect {
            Effect::Spirc(cmd) => self.send_spirc(cmd),

            Effect::StartMedia { item, epoch, gate } => {
                // Register the cancel token before the runner exists, so a
                // `CancelMedia` landing during the runner's gate wait still
                // reaches it (the feeder checks the token as it runs).
                let token = CancellationToken::new();
                self.feeder_cancel = Some(token.clone());
                self.feeder_paused.store(false, Ordering::Relaxed);

                let gate_notify = match gate {
                    StartGate::Immediate => None,
                    StartGate::AfterSpotifyPauseAck => {
                        let notify = Arc::new(Notify::new());
                        if matches!(self.state.sp, SpDevice::Paused(_)) {
                            // Already paused: no ack is coming — pre-fire.
                            notify.notify_one();
                        }
                        self.pending_gate = Some(notify.clone());
                        Some(notify)
                    }
                };

                // The core's presence effects are Spotify-only; the status
                // line's media view starts here. The fresh card posts with
                // an unpaused ⏯, so the button mirror resets with it.
                let title = item.source.display_title().to_string();
                let subtitle = item.source.display_subtitle();
                self.media_status = Some((title.clone(), subtitle.clone()));
                self.buttons_paused = false;
                let _ = self
                    .deps
                    .presence_tx
                    .send(PresenceUpdate::Playing { title, artist: subtitle });

                let ctx = RunnerCtx {
                    bridge: self.deps.bridge.clone(),
                    track_handle: self.deps.track_handle.clone(),
                    feeder_paused: self.feeder_paused.clone(),
                    dj: self.deps.dj.clone(),
                    announce_enabled: self.deps.announce_enabled.clone(),
                    ui_send: self.deps.ui_send.clone(),
                    notice_tx: self.deps.notice_tx.clone(),
                    input_tx: self.tx.clone(),
                };
                tokio::spawn(media_runner(ctx, item, epoch, token, gate_notify));
            }

            Effect::CancelMedia => {
                if let Some(token) = self.feeder_cancel.take() {
                    token.cancel();
                }
            }

            Effect::ClearBridge => self.deps.bridge.clear(),

            Effect::RecordAired(aired) => {
                // Spawned, never awaited here: the actor must not block on
                // the database. A failed write costs a history row, never
                // playback, so it is logged and dropped.
                if let Some(history) = self.deps.history.clone() {
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = history.record(&aired) {
                            tracing::warn!(error = %e, "failed to record play history");
                        }
                    });
                }
            }

            Effect::ResolveMeta(uri) => {
                // Awaited in a spawned task, never here; the answer comes
                // back through the mailbox as a gen-tagged TrackChanged.
                let spirc = { self.deps.spirc_cmd_tx.lock().clone() };
                let tx = self.tx.clone();
                let gen = self.state.link_gen;
                tokio::spawn(async move {
                    let Some(spirc) = spirc else { return };
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if spirc.send(SpircCommand::Lookup(uri.clone(), reply_tx)).is_err() {
                        return;
                    }
                    if let Ok(Some(lookup)) = reply_rx.await {
                        let meta = TrackMeta {
                            title: lookup.title,
                            artist: lookup.artist,
                            album_art_url: lookup.album_art_url,
                        };
                        let _ = tx.send(Input::Transport {
                            gen,
                            ev: TransportEvent::TrackChanged { uri, meta },
                        });
                    }
                });
            }

            Effect::LeaveVoice => {
                // Spawned, never awaited: leaving talks to Discord.
                let leave = (self.deps.leave_voice)();
                tokio::spawn(leave);
            }

            Effect::JoinVoice => {
                let join = (self.deps.join_voice)(join_hint);
                let tx = self.tx.clone();
                let notice_tx = self.deps.notice_tx.clone();
                tokio::spawn(async move {
                    if join.await {
                        let _ = tx.send(Input::VoiceReady);
                    } else {
                        let _ = notice_tx.send(
                            "⚠️ Couldn't join a voice channel, so nothing would be heard. \
                             Try again from a voice channel."
                                .to_string(),
                        );
                        // VoiceLost makes the core cancel the start it just
                        // issued instead of feeding a dead call.
                        let _ = tx.send(Input::VoiceLost);
                    }
                });
            }


            Effect::TrackHandle(cmd) => {
                let paused = matches!(cmd, TrackHandleCmd::Pause);
                // Pause the feeder too: the songbird pause freezes output
                // instantly, and the flag stops the download side from
                // racing ahead more than the bridge can hold.
                self.feeder_paused.store(paused, Ordering::Relaxed);
                let handle = { self.deps.track_handle.lock().clone() };
                if let Some(handle) = handle {
                    let _ = if paused { handle.pause() } else { handle.play() };
                }
                self.buttons_paused = paused;
                (self.deps.ui_send)(UiEvent::Buttons { paused });
                // Mirror the media item's pause state on the status line.
                let update = if paused {
                    Some(PresenceUpdate::Paused)
                } else {
                    self.media_status
                        .clone()
                        .map(|(title, artist)| PresenceUpdate::Playing { title, artist })
                };
                if let Some(update) = update {
                    let _ = self.deps.presence_tx.send(update);
                }
            }

            Effect::Ui(msg) => match msg {
                CoreUiMsg::NowPlayingMedia { item } => {
                    (self.deps.ui_send)(UiEvent::NowPlayingMedia { item });
                }
                CoreUiMsg::NowPlayingSpotify { uri, meta } => {
                    (self.deps.ui_send)(UiEvent::NowPlayingSpotify { uri, meta });
                }
                CoreUiMsg::TakeoverPrompt => {
                    let _ = self.deps.notice_tx.send(
                        "Spotify is in use on another device — press ▶ to take over.".to_string(),
                    );
                }
                CoreUiMsg::Notice(text) => {
                    let _ = self.deps.notice_tx.send(text);
                }
            },

            Effect::Presence(state) => {
                // A core presence effect is the baseline speaking for
                // itself; it supersedes any shell-fed media status.
                self.media_status = None;
                let update = match state {
                    PresenceState::Idle => PresenceUpdate::Idle,
                    PresenceState::Playing { meta, .. } => {
                        PresenceUpdate::Playing { title: meta.title, artist: meta.artist }
                    }
                    PresenceState::Paused { .. } => PresenceUpdate::Paused,
                };
                let _ = self.deps.presence_tx.send(update);
            }

            Effect::Announce(AnnounceKind::Track { title, artist }) => {
                // Spotify-track announcement (media items announce from
                // their own runner). Spawned: clip synthesis is IO.
                if self.deps.announce_enabled.load(Ordering::Relaxed) {
                    let dj = self.deps.dj.clone();
                    let bridge = self.deps.bridge.clone();
                    tokio::spawn(async move {
                        match dj.track_announce_clip(&title, &artist, "").await {
                            Some(clip) => {
                                tracing::info!(title = %title, artist = %artist, samples = clip.len(), "DJ overlay pushed");
                                bridge.push_overlay(&clip);
                            }
                            None => {
                                tracing::warn!(title = %title, artist = %artist, "DJ clip failed");
                            }
                        }
                    });
                }
            }

            Effect::SetTimer(kind, duration) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(duration).await;
                    let _ = tx.send(Input::Tick(kind));
                });
            }

            Effect::Reply(tx, text) => {
                let _ = tx.send(text);
            }

            Effect::ReplySnapshot(tx, snapshot) => {
                let _ = tx.send(snapshot);
            }
        }
    }

    /// Map a core Spirc command onto the live session's channel. Dropped
    /// with a debug log when no session is live — the core's own
    /// `device_active` gating makes that rare.
    fn send_spirc(&self, cmd: SpircCmd) {
        let tx = { self.deps.spirc_cmd_tx.lock().clone() };
        let Some(tx) = tx else {
            tracing::debug!(?cmd, "spirc command dropped (no live session)");
            return;
        };
        let command = match cmd {
            SpircCmd::Pause => SpircCommand::Pause,
            SpircCmd::Play => SpircCommand::Play,
            SpircCmd::Next => SpircCommand::Next,
            SpircCmd::Previous => SpircCommand::Previous,
            SpircCmd::AddToQueue(uri) => SpircCommand::AddToQueue(uri),
            SpircCmd::Load(uri) => SpircCommand::Load(uri),
            SpircCmd::ActivateDevice => SpircCommand::ActivateDevice,
            SpircCmd::Transfer => SpircCommand::Transfer,
            SpircCmd::Disconnect => SpircCommand::Disconnect,
        };
        let _ = tx.send(command);
    }
}

/// Everything one media runner needs, cloned out of the actor's deps.
struct RunnerCtx {
    bridge: Arc<AudioBridge>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
    feeder_paused: Arc<AtomicBool>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    ui_send: UiSendFn,
    notice_tx: mpsc::UnboundedSender<String>,
    input_tx: mpsc::UnboundedSender<Input>,
}

/// One queue item's playback, start to finish: await the start gate, resume
/// the shared bridge-reader track, run the pre-feed DJ announce, feed the
/// item into the bridge, then report `MediaEnded` with this start's epoch.
/// The runner owns nothing beyond its own feed — cancellation arrives
/// through the token the actor registered before spawning it.
async fn media_runner(
    ctx: RunnerCtx,
    item: QueueItem,
    epoch: u64,
    token: CancellationToken,
    gate: Option<Arc<Notify>>,
) {
    if let Some(gate) = gate {
        // The fallback mirrors the old fixed post-Pause sleep; the actor
        // pre-fires the gate when the baseline was already paused.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(PAUSE_ACK_FALLBACK_MS),
            gate.notified(),
        )
        .await;
    }

    // The bridge-reader track may be paused (the baseline was paused or
    // idle before this item); resume it so the feed is heard.
    {
        let handle = { ctx.track_handle.lock().clone() };
        if let Some(handle) = handle {
            let _ = handle.play();
        }
    }

    // DJ announcement before the track (honors the /announce toggle).
    if ctx.announce_enabled.load(Ordering::Relaxed) && ctx.dj.is_enabled() {
        let title = item.source.display_title().to_string();
        let subtitle = item.source.display_subtitle();
        if let Some(clip) = ctx.dj.track_announce_clip(&title, &subtitle, &item.queued_by).await {
            ctx.bridge.push_overlay(&clip);
        }
    }

    let feed_result = match &item.source {
        MediaSource::YouTube { url, .. } => {
            feed_youtube_to_bridge(url, ctx.bridge.clone(), token, ctx.feeder_paused.clone()).await
        }
        MediaSource::File { attachment_url, filename, .. } => {
            let ext = filename.rsplit('.').next().unwrap_or("mp3");
            feed_file_to_bridge(
                attachment_url,
                ext,
                ctx.bridge.clone(),
                token,
                ctx.feeder_paused.clone(),
            )
            .await
        }
        MediaSource::Spotify { .. } => {
            // The core only ever starts media heads; a Spotify item here is
            // a bug upstream, not something to feed.
            tracing::error!("media runner started with a Spotify item; refusing to feed");
            Ok(())
        }
    };

    let outcome = match feed_result {
        Ok(()) => {
            tracing::info!("priority item finished: {}", item.source.display_title());
            (ctx.ui_send)(UiEvent::HistoryMedia { item: item.clone() });
            MediaOutcome::Finished
        }
        Err(FeederError::Cancelled) => {
            tracing::info!("priority item cancelled (skip/stop)");
            MediaOutcome::Cancelled
        }
        Err(e) => {
            tracing::warn!("feeder error: {}", e);
            let _ = ctx.notice_tx.send(format!(
                "⚠️ <@{}> Couldn't play **{}** — the download or decode failed.",
                item.queued_by_id,
                item.source.display_title()
            ));
            // A failed item's card must not linger with dead buttons; the
            // boundary decision may repost right over this when the next
            // item starts.
            (ctx.ui_send)(UiEvent::IdleCard);
            MediaOutcome::Finished
        }
    };

    let _ = ctx.input_tx.send(Input::MediaEnded { epoch, outcome });
}
