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
//! deliberately doesn't model them: the bridge's music drain follows raw
//! transport telemetry while its reader stays live for overlays, and
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
use crate::audio_bridge::{AudioBridge, OverlayHandle};
use crate::player::state::{
    step, Active, AnnounceKind, Effect, EnqueuePos, Input, MediaOutcome, NowPlaying,
    PlayerSnapshot, PlayerState, PresenceState, PreviousTrack, SpDevice, SpircCmd, StartGate,
    TrackHandleCmd,
    TrackMeta, TransportEvent, VoiceGuard, UiMsg as CoreUiMsg,
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

/// Keep the existing DJ level separate from the soundboard's configured gain.
const DJ_OVERLAY_GAIN: f32 = 0.18;

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
/// reserves synchronously, then resolves the routing revision once connected.
/// A guarded account join must still match its original panel revision.
pub type JoinVoiceFn = Arc<
    dyn Fn(Option<u64>, Option<VoiceGuard>) -> Pin<Box<dyn Future<Output = Option<u64>> + Send>>
        + Send
        + Sync,
>;

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
    pub authorize_voice: Arc<dyn Fn(&VoiceGuard) -> bool + Send + Sync>,
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
    guard: Option<VoiceGuard>,
    tx: mpsc::UnboundedSender<Input>,
    /// The same spirc cell the actor holds, so `lookup_spotify` runs in the
    /// caller's task without a
    /// mailbox round-trip — and without the caller ever holding a channel it
    /// could drive playback with directly.
    spirc: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
}

impl PlayerHandle {
    pub fn guarded(&self, guard: VoiceGuard) -> Self {
        Self {
            guard: Some(guard),
            ..self.clone()
        }
    }

    /// Send a reply-less input (transport events, media/voice reports,
    /// timer ticks). Dropped silently if the actor is gone.
    pub fn send(&self, input: Input) {
        let _ = self.tx.send(input);
    }

    async fn request(&self, make: impl FnOnce(oneshot::Sender<String>) -> Input) -> String {
        let (tx, rx) = oneshot::channel();
        let input = make(tx);
        let input = match &self.guard {
            Some(guard) => Input::Guarded {
                guard: guard.clone(),
                input: Box::new(input),
            },
            None => input,
        };
        if self.tx.send(input).is_err() {
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

    /// A human `/stop`: silence everything and leave the voice channel.
    pub async fn stop(&self) -> String {
        self.request(|reply| Input::Stop { reply, leave_voice: true }).await
    }

    /// The teardown paths' stop: the same silence, but the caller owns the
    /// voice connection (already dropped by Discord, or removed by the
    /// caller itself), so no `LeaveVoice` — see `Input::Stop`.
    pub async fn stop_without_leaving(&self) -> String {
        self.request(|reply| Input::Stop { reply, leave_voice: false }).await
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

/// Store work in actor order. Previous-track reads must see every earlier
/// aired-track write, even if the user presses Back before it commits.
enum StoreRequest {
    /// The whole queue, latest-wins — an older snapshot committing last
    /// would leave the table permanently behind memory.
    Queue(Vec<QueueItem>),
    /// One aired track. Order matters here too: history row ids are what
    /// back-navigation walks, so an out-of-order insert misorders the walk.
    Aired(crate::player::state::AiredTrack),
    ResolvePrevious {
        request_id: u64,
        before: Option<i64>,
        reply: mpsc::UnboundedSender<Input>,
    },
}

/// The one thread allowed to write to the stores.
///
/// Replaces a `spawn_blocking` per write: the blocking pool gives no
/// ordering between tasks, and `QueueStore::save` is a whole-table rewrite,
/// so two saves landing out of order persist the older queue with nothing
/// to reconcile it. Queue snapshots coalesce — only the newest pending one
/// is worth writing.
fn spawn_store_worker(
    history: Option<Arc<crate::history::HistoryStore>>,
    queue_store: Option<Arc<crate::queue_store::QueueStore>>,
) -> mpsc::UnboundedSender<StoreRequest> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || run_store_worker(history, queue_store, rx));
    tx
}

fn run_store_worker(
    history: Option<Arc<crate::history::HistoryStore>>,
    queue_store: Option<Arc<crate::queue_store::QueueStore>>,
    mut rx: mpsc::UnboundedReceiver<StoreRequest>,
) {
    while let Some(first) = rx.blocking_recv() {
        // Drain whatever else is already queued so a burst of queue
        // mutations costs one rewrite rather than one per mutation.
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }
        let mut latest_queue = None;
        for request in batch {
            match request {
                StoreRequest::Queue(items) => latest_queue = Some(items),
                StoreRequest::Aired(track) => {
                    if let Some(h) = &history {
                        if let Err(e) = h.record(&track) {
                            tracing::warn!(error = %e, "failed to record play history");
                        }
                    }
                }
                StoreRequest::ResolvePrevious { request_id, before, reply } => {
                    let track = history.as_ref().and_then(|history| {
                        match history.aired_before(before, |row| {
                            SpotifyUri::from_uri(&row.track_ref).is_ok()
                        }) {
                            Ok(row) => row.and_then(|row| {
                                SpotifyUri::from_uri(&row.track_ref).ok().map(|uri| {
                                    PreviousTrack {
                                        id: row.id,
                                        uri,
                                        context_uri: row.context_uri,
                                    }
                                })
                            }),
                            Err(e) => {
                                tracing::warn!(error = %e, "could not read the play history");
                                None
                            }
                        }
                    });
                    let _ = reply.send(Input::PreviousResolved { request_id, track, authorized: true });
                }
            }
        }
        if let (Some(items), Some(store)) = (latest_queue, &queue_store) {
            if let Err(e) = store.save(&items) {
                tracing::warn!(error = %e, "failed to persist the queue");
            }
        }
    }
}

/// Spawn the player actor and return its handle. Called once, when the bot
/// is built; the actor lives for the process (lifecycle A).
pub fn spawn(deps: PlayerDeps) -> PlayerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = PlayerHandle {
        guard: None,
        tx: tx.clone(),
        spirc: deps.spirc_cmd_tx.clone(),
    };
    let store_tx = spawn_store_worker(deps.history.clone(), deps.queue_store.clone());
    let actor = Actor {
        deps,
        store_tx,
        tx,
        state: PlayerState::new(),
        previous_authorization: None,
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
    /// Durable writes and ordered history reads; see [`spawn_store_worker`].
    store_tx: mpsc::UnboundedSender<StoreRequest>,
    /// Own mailbox sender, cloned into runners/timers/join tasks so their
    /// completions come back as inputs.
    tx: mpsc::UnboundedSender<Input>,
    state: PlayerState,
    /// Voice membership captured for the one Back read still allowed to finish.
    previous_authorization: Option<PreviousAuthorization>,
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
            let Some((mut input, guard)) = authorize_input(input, &*self.deps.authorize_voice)
            else {
                continue;
            };
            let requester = guard.as_ref().map(|guard| guard.user);
            authorize_previous_completion(
                &mut input,
                &mut self.previous_authorization,
                &*self.deps.authorize_voice,
            );
            reset_audio_for_input(&self.deps.bridge, &input);
            // Shell-side handling that reads raw inputs rather than core
            // decisions, run on receipt before the step.
            match &input {
                Input::Transport { gen, ev } if *gen == self.state.link_gen => {
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
            // Who to follow into voice, when this input turns out to need a
            // join. Read from the input rather than only from the effects
            // because a Spotify enqueue produces no `StartMedia` — it reaches
            // the join through `begin_load` — so hint-from-effects alone sent
            // a cold `/play <spotify link>` to the configured channel while
            // the same command with a YouTube link followed the requester.
            let enqueued_by = match &input {
                Input::Enqueue { item, .. } => Some(item.queued_by_id),
                _ => None,
            };
            let queue_rev = self.state.queue.revision();
            let effects = step(&mut self.state, input, Instant::now());
            if let Some(request_id) = effects.iter().find_map(|effect| match effect {
                Effect::ResolvePrevious { request_id, .. } => Some(*request_id),
                _ => None,
            }) {
                self.previous_authorization = Some(PreviousAuthorization { request_id, guard });
            }
            if self.previous_authorization.as_ref().is_some_and(|pending| {
                Some(pending.request_id) != self.state.pending_previous_request_id()
            }) {
                self.previous_authorization = None;
            }
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
                let _ = self
                    .store_tx
                    .send(StoreRequest::Queue(self.state.queue.snapshot()));
            }

            // A `StartMedia` in this batch names whose item is airing, which
            // beats the enqueuer when they differ — the item starting is the
            // one the bot is joining for.
            let mut join_hint = enqueued_by;
            for effect in &effects {
                if let Effect::StartMedia { item, .. } = effect {
                    join_hint = Some(item.queued_by_id);
                }
            }

            for effect in effects {
                self.run_effect(effect, requester.or(join_hint));
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

    /// Gate music drainage from raw Spotify telemetry. The shared reader
    /// stays live so overlays remain audible while music is paused; a media
    /// item's own pause state takes precedence over Spotify telemetry.
    /// Also mirrors the baseline's pause state onto the card's ⏯ button
    /// (only while the baseline owns the
    /// card — a pause echo under a media item must not repaint its button).
    fn drive_reader(&mut self, ev: &TransportEvent) {
        let media_turn = matches!(self.state.active, Active::Media { .. });
        if let Some(paused) = music_pause_for_transport(&self.state.active, ev) {
            self.deps.bridge.set_music_paused(paused);
        }
        match ev {
            TransportEvent::Playing { .. } => {
                // The music gate above still protects a paused media item
                // when Spotify reports an interloping play.
                let handle = { self.deps.track_handle.lock().clone() };
                if let Some(handle) = handle {
                    let _ = handle.play();
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
                self.deps.bridge.set_music_paused(false);

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

            // Music transitions fence delayed DJ synthesis, but a soundboard
            // clip already mixing into this call keeps playing. Explicit
            // stop/voice loss clears both lanes before the core runs.
            Effect::ClearBridge => self.deps.bridge.clear_music(),

            Effect::RecordAired(aired) => {
                // Handed to the serialized writer, never written here: the
                // actor must not block on the database, and row order is the
                // order back-navigation walks.
                let _ = self.store_tx.send(StoreRequest::Aired(aired));
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

            Effect::ResolvePrevious { request_id, before } => {
                // The same FIFO as RecordAired: a lookup cannot race ahead
                // of the row for the track the room is hearing right now.
                // Sending stays synchronous; the worker returns via our mailbox.
                if self.store_tx.send(StoreRequest::ResolvePrevious {
                    request_id,
                    before,
                    reply: self.tx.clone(),
                }).is_err() {
                    let _ = self.tx.send(Input::PreviousResolved { request_id, track: None, authorized: true });
                }
            }

            Effect::LeaveVoice => {
                // Spawned, never awaited: leaving talks to Discord.
                let leave = (self.deps.leave_voice)();
                tokio::spawn(leave);
            }

            Effect::JoinVoice { generation } => {
                let join = (self.deps.join_voice)(join_hint, None);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let ready = join.await.is_some();
                    let _ = tx.send(Input::VoiceJoinFinished { generation, ready });
                });
            }


            Effect::TrackHandle(cmd) => {
                let paused = matches!(cmd, TrackHandleCmd::Pause);
                // Freeze only music: the shared Songbird track stays live
                // for soundboard/DJ overlays, and the feeder stops advancing
                // beyond the music already buffered.
                self.feeder_paused.store(paused, Ordering::Relaxed);
                self.deps.bridge.set_music_paused(paused);
                let handle = { self.deps.track_handle.lock().clone() };
                if let Some(handle) = handle {
                    let _ = handle.play();
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
                    let overlay_epoch = bridge.overlay_epoch();
                    tokio::spawn(async move {
                        match dj.track_announce_clip(&title, &artist, "").await {
                            Some(clip) => {
                                if let Err(error) = bridge.start_overlay(overlay_epoch, clip, DJ_OVERLAY_GAIN) {
                                    tracing::debug!(?error, "dj overlay skipped");
                                }
                            }
                            None => {
                                tracing::warn!(title = %title, artist = %artist, "dj clip failed");
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
            SpircCmd::LoadContext { context_uri, track_uri, options } => {
                SpircCommand::LoadContext {
                    context_uri,
                    track_uri,
                    options: options
                        .map(|o| (o.shuffle, o.repeat_context, o.repeat_track)),
                }
            }
        };
        let _ = tx.send(command);
    }
}

fn reset_audio_for_input(bridge: &AudioBridge, input: &Input) {
    if matches!(input, Input::Stop { .. } | Input::VoiceLost) {
        bridge.clear();
    }
}

/// Spotify echoes may not release a media item's human pause. All other
/// transport pause decisions affect only music; overlay consumption is live.
fn music_pause_for_transport(active: &Active, ev: &TransportEvent) -> Option<bool> {
    if let Active::Media { paused, .. } = active {
        return matches!(ev, TransportEvent::Playing { .. }).then_some(*paused);
    }
    match ev {
        TransportEvent::Playing { .. } => Some(false),
        TransportEvent::Paused { .. }
        | TransportEvent::Stopped
        | TransportEvent::EndOfTrack
        | TransportEvent::Unavailable { .. } => Some(true),
        _ => None,
    }
}

/// Everything one media runner needs, cloned out of the actor's deps.
struct RunnerCtx {
    bridge: Arc<AudioBridge>,
    feeder_paused: Arc<AtomicBool>,
    dj: Arc<DJAnnouncer>,
    announce_enabled: Arc<AtomicBool>,
    ui_send: UiSendFn,
    notice_tx: mpsc::UnboundedSender<String>,
    input_tx: mpsc::UnboundedSender<Input>,
}

/// A cancelled runner can clean up only its own announcement, including
/// cancellation racing between synthesis completion and overlay admission.
struct RunnerOverlay {
    bridge: Arc<AudioBridge>,
    handle: OverlayHandle,
    token: CancellationToken,
}

impl Drop for RunnerOverlay {
    fn drop(&mut self) {
        if self.token.is_cancelled() {
            self.bridge.cancel_overlay(&self.handle);
        }
    }
}

/// One queue item's playback, start to finish: await the start gate, run
/// the pre-feed DJ announce, feed the item into the bridge, then report
/// `MediaEnded` with this start's epoch.
/// The runner owns nothing beyond its own feed — cancellation arrives
/// through the token the actor registered before spawning it.
async fn media_runner(
    ctx: RunnerCtx,
    item: QueueItem,
    epoch: u64,
    token: CancellationToken,
    gate: Option<Arc<Notify>>,
) {
    complete_media_runner(
        &ctx.input_tx,
        epoch,
        run_media(&ctx, &item, token, gate),
    ).await;
}

/// Every normal exit, including cancellation before feeding starts, must
/// reach the actor: Skip waits for this epoch's completion to advance.
async fn complete_media_runner(
    input_tx: &mpsc::UnboundedSender<Input>,
    epoch: u64,
    run: impl Future<Output = MediaOutcome>,
) {
    let outcome = run.await;
    let _ = input_tx.send(Input::MediaEnded { epoch, outcome });
}

/// Own all asynchronous setup before feeding. The announcement future is
/// lazy, so neither synthesis nor overlay admission starts before the gate.
async fn prepare_media_runner(
    bridge: &Arc<AudioBridge>,
    token: &CancellationToken,
    gate: Option<Arc<Notify>>,
    announcement: impl Future<Output = Option<Vec<f32>>>,
) -> Result<Option<RunnerOverlay>, MediaOutcome> {
    if let Some(gate) = gate {
        // The fallback mirrors the old fixed post-Pause sleep; the actor
        // pre-fires the gate when the baseline was already paused.
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(MediaOutcome::Cancelled),
            _ = tokio::time::timeout(
                std::time::Duration::from_millis(PAUSE_ACK_FALLBACK_MS),
                gate.notified(),
            ) => {}
        }
    }

    // The actor owns the music pause gate. A delayed runner must never
    // reopen it after a user pause, skip, stop, or replacement start.
    if token.is_cancelled() {
        return Err(MediaOutcome::Cancelled);
    }

    let overlay_epoch = bridge.overlay_epoch();
    let clip = tokio::select! {
        biased;
        _ = token.cancelled() => return Err(MediaOutcome::Cancelled),
        clip = announcement => clip,
    };
    let owned = if let Some(clip) = clip {
        match bridge.start_overlay(overlay_epoch, clip, DJ_OVERLAY_GAIN) {
            Ok(handle) => Some(RunnerOverlay {
                bridge: bridge.clone(),
                handle,
                token: token.clone(),
            }),
            Err(error) => {
                tracing::debug!(?error, "dj overlay skipped");
                None
            }
        }
    } else {
        None
    };
    if token.is_cancelled() {
        return Err(MediaOutcome::Cancelled);
    }
    Ok(owned)
}

async fn run_media(
    ctx: &RunnerCtx,
    item: &QueueItem,
    token: CancellationToken,
    gate: Option<Arc<Notify>>,
) -> MediaOutcome {
    // DJ announcement before the track (honors the /announce toggle).
    let announcement = async {
        if ctx.announce_enabled.load(Ordering::Relaxed) && ctx.dj.is_enabled() {
            let title = item.source.display_title().to_string();
            let subtitle = item.source.display_subtitle();
            ctx.dj.track_announce_clip(&title, &subtitle, &item.queued_by).await
        } else {
            None
        }
    };
    let _announcement = match prepare_media_runner(&ctx.bridge, &token, gate, announcement).await {
        Ok(owned) => owned,
        Err(outcome) => return outcome,
    };

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

    match feed_result {
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
    }
}

struct PreviousAuthorization {
    request_id: u64,
    guard: Option<VoiceGuard>,
}

/// A stale result must never consume a newer caller's authorization. Trusted
/// internal requests have no guard; guarded requests recheck current membership.
fn authorize_previous_completion(
    input: &mut Input,
    pending: &mut Option<PreviousAuthorization>,
    authorize: &dyn Fn(&VoiceGuard) -> bool,
) {
    let Input::PreviousResolved { request_id, authorized, .. } = input else {
        return;
    };
    if !pending.as_ref().is_some_and(|pending| pending.request_id == *request_id) {
        *authorized = false;
        return;
    }
    let pending = pending.take().expect("request matched");
    *authorized &= pending.guard.as_ref().is_none_or(authorize);
}

fn authorize_input(
    input: Input,
    authorize: &dyn Fn(&VoiceGuard) -> bool,
) -> Option<(Input, Option<VoiceGuard>)> {
    match input {
        Input::Guarded { guard, input } => {
            if !authorize(&guard) {
                reject_guarded(*input);
                return None;
            }
            Some((*input, Some(guard)))
        }
        input => Some((input, None)),
    }
}

/// Guard failure releases the waiting command without ever touching playback.
fn reject_guarded(input: Input) {
    let reply = match input {
        Input::Enqueue { reply, .. }
        | Input::Play { reply }
        | Input::Skip { reply }
        | Input::Stop { reply, .. }
        | Input::TogglePause { reply }
        | Input::Previous { reply }
        | Input::ClearQueue { reply } => reply,
        _ => return,
    };
    let _ = reply.send("Your voice room or this music session changed. Open a fresh menu.".into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_bridge::{OverlayError, OverlayStatus};
    use crate::history::HistoryStore;
    use crate::player::state::{AiredSource, AiredTrack};

    async fn cancelled_preparation(
        bridge: &Arc<AudioBridge>,
        token: &CancellationToken,
        gate: Option<Arc<Notify>>,
        announcement: impl Future<Output = Option<Vec<f32>>>,
    ) -> MediaOutcome {
        match prepare_media_runner(bridge, token, gate, announcement).await {
            Err(outcome) => outcome,
            Ok(_) => panic!("cancelled preparation must not reach the feeder"),
        }
    }

    fn assert_cancelled_completion(rx: &mut mpsc::UnboundedReceiver<Input>, epoch: u64) {
        let completion = rx.try_recv().expect("the actor must receive the cancellation");
        assert!(matches!(
            &completion,
            Input::MediaEnded { epoch: received, outcome: MediaOutcome::Cancelled } if *received == epoch
        ));
        assert!(rx.try_recv().is_err(), "one completion per runner");

        // The real consumer depends on this report: Skip cancels the runner
        // first and starts the next queued item only after MediaEnded arrives.
        let item = |name: &str| QueueItem::new(
            MediaSource::File {
                filename: name.into(),
                attachment_url: "https://example.invalid/test.wav".into(),
            },
            "test".into(),
            1,
        );
        let mut state = PlayerState::new();
        state.active = Active::Media { item: item("first.wav"), paused: false, epoch };
        state.media_epoch = epoch;
        assert!(state.queue.push(item("next.wav")));
        let (reply, _) = oneshot::channel();
        let now = Instant::now();
        let skip = step(&mut state, Input::Skip { reply }, now);
        assert!(skip.iter().any(|effect| matches!(effect, Effect::CancelMedia)));
        let completed = step(&mut state, completion, now);
        assert!(completed.iter().any(|effect| matches!(
            effect,
            Effect::StartMedia { item, .. } if item.source.display_title() == "next.wav"
        )), "cancellation completion releases the next queued item");
    }

    #[tokio::test]
    async fn cancellation_before_feeding_reports_completion_with_or_without_a_start_gate() {
        for wait_for_gate in [false, true] {
            let bridge = AudioBridge::new(1);
            let token = CancellationToken::new();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let gate = wait_for_gate.then(|| Arc::new(Notify::new()));
            let runner = complete_media_runner(&tx, 42, cancelled_preparation(
                &bridge,
                &token,
                gate,
                async { panic!("a cancelled start must never begin synthesis") },
            ));
            tokio::pin!(runner);
            if wait_for_gate {
                // Poll once to place the real preparation future inside its
                // pending pause-ack wait, without a wall-clock sleep.
                let pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(runner.as_mut().poll(cx).is_pending())
                }).await;
                assert!(pending);
            }
            token.cancel();
            runner.await;
            assert_cancelled_completion(&mut rx, 42);
            assert!(!bridge.has_overlay_audio());
        }
    }

    #[tokio::test]
    async fn cancellation_during_synthesis_reaches_the_actor_mailbox() {
        let bridge = AudioBridge::new(1);
        let token = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (started, synthesis_started) = oneshot::channel();
        let announcement = async {
            let _ = started.send(());
            std::future::pending::<Option<Vec<f32>>>().await
        };
        tokio::join!(
            complete_media_runner(&tx, 73, cancelled_preparation(&bridge, &token, None, announcement)),
            async {
                synthesis_started.await.unwrap();
                token.cancel();
            },
        );
        assert_cancelled_completion(&mut rx, 73);
        assert!(!bridge.has_overlay_audio());
    }

    #[tokio::test]
    async fn cancellation_racing_synthesis_completion_reports_and_removes_its_overlay() {
        let bridge = AudioBridge::new(1);
        let token = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let announcement = async {
            // The cancellation branch was polled first while still live;
            // synthesis then completes at the same instant as the cancel.
            token.cancel();
            Some(vec![0.25, 0.25])
        };
        complete_media_runner(&tx, 91, cancelled_preparation(&bridge, &token, None, announcement)).await;
        assert_cancelled_completion(&mut rx, 91);
        assert!(!bridge.has_overlay_audio(), "cancelled setup owns no audible clip");
    }

    #[test]
    fn spotify_telemetry_cannot_unpause_media_or_silence_its_overlay() {
        let bridge = AudioBridge::new(1);
        bridge.push_samples(&[0.75, 0.75]);
        let item = QueueItem::new(
            MediaSource::File {
                filename: "test.wav".into(),
                attachment_url: "https://example.invalid/test.wav".into(),
            },
            "test".into(),
            1,
        );
        let active = Active::Media { item, paused: true, epoch: 1 };
        let uri = SpotifyUri::from_uri("spotify:track:0000000000000000000001").unwrap();
        let playing = TransportEvent::Playing { uri: uri.clone(), meta: None };
        bridge.set_music_paused(music_pause_for_transport(&active, &playing).unwrap());
        assert_eq!(music_pause_for_transport(&active, &TransportEvent::Paused { uri }), None);
        bridge.start_overlay(bridge.overlay_epoch(), vec![0.25, 0.25], 1.0).unwrap();
        let mut output = [0.0; 2];
        assert_eq!(bridge.pull_samples(&mut output), 2);
        assert_eq!(output, [0.25, 0.25]);
        assert_eq!(bridge.len(), 2, "media samples stay frozen for resume");
        bridge.set_music_paused(false);
        assert_eq!(bridge.pull_samples(&mut output), 2);
        assert!(output[0] > 0.0 && output[0] <= 0.75);
        assert_eq!(output[0], output[1]);
        assert_eq!(bridge.len(), 0, "resume consumes the retained frame");
        bridge.pull_samples(&mut vec![0.0; crate::audio_bridge::SAMPLE_RATE * 2]);
        bridge.push_samples(&[0.75, 0.75]);
        bridge.pull_samples(&mut output);
        assert_eq!(output, [0.75, 0.75], "music returns to unity after the duck release");
    }

    #[test]
    fn stop_and_voice_loss_cancel_audio_and_reject_delayed_dj_synthesis() {
        let (reply, _) = oneshot::channel();
        for input in [Input::Stop { reply, leave_voice: true }, Input::VoiceLost] {
            let bridge = AudioBridge::new(1);
            bridge.push_samples(&[0.5, 0.5]);
            let clip = bridge.start_overlay(bridge.overlay_epoch(), vec![0.25, 0.25], 1.0).unwrap();
            let synthesis_epoch = bridge.overlay_epoch();
            reset_audio_for_input(&bridge, &input);
            assert_eq!(bridge.len(), 0);
            assert_eq!(clip.status(), OverlayStatus::Cancelled);
            assert!(matches!(
                bridge.start_overlay(synthesis_epoch, vec![0.25, 0.25], DJ_OVERLAY_GAIN),
                Err(OverlayError::Stale)
            ));
        }
    }

    #[test]
    fn cancelled_runner_cleans_its_announcement_without_touching_a_replacement() {
        for replace in [false, true] {
            let bridge = AudioBridge::new(1);
            let token = CancellationToken::new();
            let first = bridge.start_overlay(bridge.overlay_epoch(), vec![0.25, 0.25], DJ_OVERLAY_GAIN).unwrap();
            let owned = RunnerOverlay { bridge: bridge.clone(), handle: first.clone(), token: token.clone() };
            let replacement = if replace {
                bridge.clear();
                Some(bridge.start_overlay(bridge.overlay_epoch(), vec![0.5, 0.5], 1.0).unwrap())
            } else {
                None
            };
            token.cancel();
            drop(owned);
            assert_eq!(first.status(), OverlayStatus::Cancelled);
            if let Some(replacement) = replacement {
                assert_eq!(replacement.status(), OverlayStatus::Playing);
            }
        }
    }

    #[test]
    fn spotify_pause_leaves_a_running_soundboard_clip_alive() {
        let bridge = AudioBridge::new(1);
        bridge.push_samples(&[0.5, 0.5]);
        let clip = bridge.start_overlay(bridge.overlay_epoch(), vec![0.25, 0.25], 1.0).unwrap();
        let ev = TransportEvent::Paused {
            uri: SpotifyUri::from_uri("spotify:track:0000000000000000000001").unwrap(),
        };
        bridge.set_music_paused(music_pause_for_transport(&Active::Spotify { track: None }, &ev).unwrap());
        reset_audio_for_input(&bridge, &Input::Transport { gen: 1, ev });
        bridge.clear_music();
        assert_eq!(clip.status(), OverlayStatus::Playing);
        let mut output = [0.0; 2];
        assert_eq!(bridge.pull_samples(&mut output), 2);
        assert_eq!(output, [0.25, 0.25]);
    }

    #[test]
    fn queued_command_rechecks_membership_when_the_actor_receives_it() {
        let (reply, mut received) = oneshot::channel();
        let guard = VoiceGuard {
            generation: 3,
            room: 10,
            user: 1,
            may_join: true,
        };
        let input = Input::Guarded {
            guard,
            input: Box::new(Input::Stop {
                reply,
                leave_voice: true,
            }),
        };
        // The caller was in room 10 when it sent the message, but has left
        // by the time the actor consumes it. No playback input may emerge.
        assert!(authorize_input(input, &|guard| guard.allows(3, Some(10), None)).is_none());
        assert!(received.try_recv().unwrap().contains("voice room"));
    }

    #[test]
    fn another_room_claiming_voice_invalidates_a_queued_idle_request() {
        let (reply, mut received) = oneshot::channel();
        let guard = VoiceGuard {
            generation: 1,
            room: 10,
            user: 1,
            may_join: true,
        };
        let input = Input::Guarded {
            guard,
            input: Box::new(Input::Play { reply }),
        };
        assert!(authorize_input(input, &|guard| guard.allows(2, Some(20), Some(10))).is_none());
        assert!(received.try_recv().is_ok());
    }

    #[test]
    fn late_account_activation_cannot_undo_stop() {
        let guard = VoiceGuard {
            generation: 3,
            room: 10,
            user: 1,
            may_join: false,
        };
        let input = Input::Guarded {
            guard,
            input: Box::new(Input::ActivateDevice),
        };
        assert!(authorize_input(input, &|guard| guard.allows(4, None, Some(10))).is_none());
    }

    #[test]
    fn back_completion_rechecks_the_requesters_voice_before_navigation() {
        for (generation, bot_room, user_room, allowed) in [
            (3, Some(10), Some(10), true),
            (3, Some(10), None, false),
            (3, Some(10), Some(20), false),
            (4, Some(20), Some(10), false),
        ] {
            let mut state = PlayerState::new();
            state.device_active = true;
            state.sp = SpDevice::Playing(SpotifyUri::from_uri(&aired(9).track_ref).unwrap());
            state.active = Active::Spotify { track: None };
            let (reply, mut received) = oneshot::channel();
            let (input, guard) = authorize_input(Input::Guarded {
                guard: VoiceGuard { generation: 3, room: 10, user: 1, may_join: false },
                input: Box::new(Input::Previous { reply }),
            }, &|guard| guard.allows(3, Some(10), Some(10))).unwrap();
            let now = Instant::now();
            let effects = step(&mut state, input, now);
            let request_id = effects.iter().find_map(|effect| match effect {
                Effect::ResolvePrevious { request_id, .. } => Some(*request_id),
                _ => None,
            }).expect("the admitted Back starts a history read");
            let mut pending = Some(PreviousAuthorization { request_id, guard });
            let mut completion = Input::PreviousResolved {
                request_id,
                track: Some(PreviousTrack {
                    id: 7,
                    uri: SpotifyUri::from_uri(&aired(7).track_ref).unwrap(),
                    context_uri: Some("spotify:playlist:test".into()),
                }),
                authorized: true,
            };
            authorize_previous_completion(&mut completion, &mut pending,
                &|guard| guard.allows(generation, bot_room, user_room));
            let effects = step(&mut state, completion, now);
            assert_eq!(effects.iter().any(|effect| matches!(effect,
                Effect::Spirc(SpircCmd::LoadContext { .. }))), allowed);
            for effect in effects {
                if let Effect::Reply(reply, text) = effect {
                    reply.send(text).unwrap();
                }
            }
            assert!(!received.try_recv().expect("the caller always gets a reply").is_empty());
            assert!(pending.is_none());
        }
    }

    #[test]
    fn stale_back_completion_cannot_steal_a_newer_callers_voice_guard() {
        let mut pending = Some(PreviousAuthorization {
            request_id: 2,
            guard: Some(VoiceGuard { generation: 3, room: 10, user: 1, may_join: false }),
        });
        let mut stale = Input::PreviousResolved { request_id: 1, track: None, authorized: true };
        authorize_previous_completion(&mut stale, &mut pending,
            &|_| panic!("a stale completion cannot authorize against another caller"));
        assert!(matches!(stale, Input::PreviousResolved { authorized: false, .. }));
        assert_eq!(pending.as_ref().unwrap().request_id, 2);
        let mut current = Input::PreviousResolved { request_id: 2, track: None, authorized: true };
        authorize_previous_completion(&mut current, &mut pending,
            &|guard| guard.allows(3, Some(10), Some(10)));
        assert!(matches!(current, Input::PreviousResolved { authorized: true, .. }));
        assert!(pending.is_none());
        authorize_previous_completion(&mut current, &mut pending,
            &|_| panic!("a duplicate completion cannot reuse authorization"));
        assert!(matches!(current, Input::PreviousResolved { authorized: false, .. }));
    }

    #[test]
    fn trusted_internal_back_reads_preserve_existing_rejection() {
        for authorized in [false, true] {
            let mut pending = Some(PreviousAuthorization { request_id: 1, guard: None });
            let mut input = Input::PreviousResolved { request_id: 1, track: None, authorized };
            authorize_previous_completion(&mut input, &mut pending,
                &|_| panic!("an internal request has no Discord caller"));
            assert!(matches!(input, Input::PreviousResolved { authorized: result, .. } if result == authorized));
            assert!(pending.is_none());
        }
    }

    fn aired(n: u64) -> AiredTrack {
        AiredTrack {
            source: AiredSource::Baseline,
            track_ref: format!("spotify:track:{n:022}"),
            context_uri: Some("spotify:playlist:test".into()),
            title: None,
            artist: None,
            queued_by: None,
            queued_by_id: None,
        }
    }

    fn lookup(before: Option<i64>, reply: &mpsc::UnboundedSender<Input>) -> StoreRequest {
        StoreRequest::ResolvePrevious { request_id: 1, before, reply: reply.clone() }
    }

    fn resolved(rx: &mut mpsc::UnboundedReceiver<Input>) -> Option<PreviousTrack> {
        match rx.try_recv().expect("every lookup must answer") {
            Input::PreviousResolved { track, .. } => track,
            _ => panic!("unexpected worker reply"),
        }
    }

    fn run_batch(history: Option<Arc<HistoryStore>>, requests: Vec<StoreRequest>) {
        // Queue everything before the worker starts. This deterministically
        // tests interleaved reads/writes in one drained batch, without sleeps
        // or depending on which OS thread happens to win a race.
        let (tx, rx) = mpsc::unbounded_channel();
        for request in requests {
            assert!(tx.send(request).is_ok());
        }
        drop(tx);
        run_store_worker(history, None, rx);
    }

    #[test]
    fn history_replies_keep_distinct_request_ids_for_populated_empty_and_missing_stores() {
        for mode in 0..3 {
            let history = (mode != 0).then(|| Arc::new(HistoryStore::open(":memory:").unwrap()));
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut requests = Vec::new();
            if mode == 2 {
                requests.extend([StoreRequest::Aired(aired(1)), StoreRequest::Aired(aired(2))]);
            }
            requests.extend([
                StoreRequest::ResolvePrevious { request_id: 17, before: None, reply: tx.clone() },
                StoreRequest::ResolvePrevious { request_id: 42, before: Some(1), reply: tx },
            ]);
            run_batch(history, requests);
            for (expected_id, expected_track) in [(17, (mode == 2).then_some(1)), (42, None)] {
                match rx.try_recv().expect("every lookup answers in FIFO order") {
                    Input::PreviousResolved { request_id, track, authorized: true } => {
                        assert_eq!(request_id, expected_id);
                        assert_eq!(track.map(|track| track.id), expected_track);
                    }
                    _ => panic!("unexpected store reply"),
                }
            }
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn a_back_read_sees_earlier_airings_but_not_later_ones_in_the_same_batch() {
        let history = Arc::new(HistoryStore::open(":memory:").unwrap());
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_batch(Some(history), vec![
            StoreRequest::Aired(aired(1)),
            StoreRequest::Aired(aired(2)),
            lookup(None, &tx),
            StoreRequest::Aired(aired(3)),
            lookup(None, &tx),
        ]);
        let first = resolved(&mut rx).unwrap();
        assert_eq!(first.uri.to_uri(), aired(1).track_ref);
        assert_eq!(first.context_uri, aired(1).context_uri);
        assert_eq!(resolved(&mut rx).unwrap().uri.to_uri(), aired(2).track_ref);
    }

    #[test]
    fn back_reads_skip_more_than_fifty_media_rows_and_bad_references() {
        let history = Arc::new(HistoryStore::open(":memory:").unwrap());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut requests = vec![StoreRequest::Aired(aired(1))];
        for n in 0..60 {
            let mut media = aired(n);
            media.source = AiredSource::Request;
            media.track_ref = format!("https://example.invalid/media/{n}");
            requests.push(StoreRequest::Aired(media));
        }
        let mut invalid = aired(9);
        invalid.track_ref = "spotify:track:!".into();
        requests.push(StoreRequest::Aired(invalid));
        requests.push(StoreRequest::Aired(aired(2)));
        requests.push(lookup(None, &tx));
        run_batch(Some(history), requests);
        assert_eq!(resolved(&mut rx).unwrap().uri.to_uri(), aired(1).track_ref);
    }

    #[test]
    fn a_replay_does_not_move_an_explicit_history_cursor_forward() {
        let history = Arc::new(HistoryStore::open(":memory:").unwrap());
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_batch(Some(history), vec![
            StoreRequest::Aired(aired(1)),
            StoreRequest::Aired(aired(2)),
            StoreRequest::Aired(aired(3)),
            lookup(None, &tx),
            StoreRequest::Aired(aired(2)),
            lookup(Some(2), &tx),
            lookup(Some(1), &tx),
        ]);
        assert_eq!(resolved(&mut rx).unwrap().id, 2);
        assert_eq!(resolved(&mut rx).unwrap().id, 1);
        assert_eq!(resolved(&mut rx), None);
    }

    #[test]
    fn missing_empty_and_media_only_history_all_answer_without_a_track() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_batch(None, vec![lookup(None, &tx)]);
        assert_eq!(resolved(&mut rx), None);
        let history = Arc::new(HistoryStore::open(":memory:").unwrap());
        let mut media = aired(1);
        media.track_ref = "https://example.invalid/media".into();
        run_batch(Some(history), vec![
            lookup(None, &tx),
            StoreRequest::Aired(media.clone()),
            lookup(None, &tx),
            StoreRequest::Aired(media),
            lookup(None, &tx),
        ]);
        for _ in 0..3 {
            assert_eq!(resolved(&mut rx), None);
        }
    }

    #[tokio::test]
    async fn the_spawned_worker_returns_history_through_the_player_mailbox() {
        let history = Arc::new(HistoryStore::open(":memory:").unwrap());
        let store = spawn_store_worker(Some(history), None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(store.send(StoreRequest::Aired(aired(1))).is_ok());
        assert!(store.send(StoreRequest::Aired(aired(2))).is_ok());
        assert!(store.send(lookup(None, &tx)).is_ok());
        drop(store);
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await.expect("worker must answer").unwrap();
        match reply {
            Input::PreviousResolved { request_id: 1, track: Some(track), authorized: true } => assert_eq!(track.id, 1),
            _ => panic!("expected the track before the latest queued airing"),
        }
    }
}
