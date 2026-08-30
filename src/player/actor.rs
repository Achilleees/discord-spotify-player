//! Player actor: the impure shell around the pure core in [`super::state`].
//!
//! One task owns a [`PlayerState`] and a mailbox of [`Input`]s; everything
//! else talks to it through a [`PlayerHandle`]. Each input runs one
//! `step(state, input, now)` and then the returned effects, in order.
//! **The actor awaits nothing, ever**: every effect is a synchronous channel
//! send, an atomic store, a `CancellationToken::cancel`, or a `tokio::spawn`
//! (media runners, voice joins, timers), so a step can never park the
//! mailbox behind IO. Asynchronous completions come back as inputs —
//! `MediaEnded` tagged with its epoch, `VoiceReady`/`VoiceLost` from the
//! join task, `Tick` from spawned timers — so stale reports are ignored by
//! the core, not raced by the shell.
//!
//! ## C3 split — temporary, flipped in C5
//!
//! The actor fully owns the media path: the runner, the feeder
//! cancel/pause flags, and (through the core) the turn. But the *queue* and
//! the *armed track* are still the Handler-shared mutexes the presence loop
//! reads and writes, so this shell bridges the two worlds until C5 moves
//! ownership into the core:
//!
//! - `step` runs against the shared `priority_queue` (swapped into
//!   `state.queue` around each call, under the queue lock — `step` is
//!   synchronous, so the lock is held for microseconds and never across an
//!   await), so every push/pop lands in the one queue the presence loop and
//!   the queue listing read.
//! - `Effect::Spirc(AddToQueue)` joins the presence loop's arming critical
//!   section on the shared `armed_spotify` mutex: whoever takes the slot
//!   first arms, so a track is never `AddToQueue`'d twice even with two
//!   arming decision-makers alive.
//! - `Effect::Ui(NowPlayingSpotify)`, `Effect::Announce` and
//!   `Effect::Presence(Playing)` are suppressed here: the presence loop
//!   still owns Spotify cards, DJ announcements and `Playing` bookkeeping
//!   (queue pop + re-arm), and forwarding them too would double every one
//!   of them.
//! - `SpircCmd::ActivateDevice`/`Transfer` have no transport yet; they log
//!   and drop until C6 grows the `SpircCommand` enum.

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
use crate::config::Config;
use crate::player::state::{
    step, Effect, EnqueuePos, Input, MediaOutcome, PlayerState, PresenceState, SpDevice, SpircCmd,
    StartGate, TrackHandleCmd, TransportEvent, UiMsg as CoreUiMsg,
};
use crate::presence::PresenceUpdate;
use crate::queue::{MediaSource, PriorityQueue, QueueItem};
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

/// Everything the actor owns or bridges to. Shared `Arc`s are exactly the
/// C3-transitional surfaces described in the module docs; the rest is the
/// actor's own equipment.
pub struct PlayerDeps {
    pub bridge: Arc<AudioBridge>,
    /// Reserved for the C4+ effects (device naming in takeover prompts);
    /// carried from day one so `spawn`'s signature doesn't churn.
    pub config: Arc<Config>,
    pub ui_send: UiSendFn,
    /// Plain text-channel notices (failure messages, takeover prompts); a
    /// bot-layer task does the actual Discord send.
    pub notice_tx: mpsc::UnboundedSender<String>,
    /// Legacy presence-loop feed, still the owner of bot status and Spotify
    /// cards in C3.
    pub presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
    pub join_voice: JoinVoiceFn,
    /// The live session's command channel; `None` between sessions.
    pub spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    /// C3: still the source of truth for queue contents (see module docs).
    pub priority_queue: Arc<Mutex<PriorityQueue>>,
    /// C3: still the arming slot the presence loop checks and sets.
    pub armed_spotify: Arc<Mutex<Option<SpotifyUri>>>,
    /// C3: still read by the presence loop ("is a media item audible") and
    /// the teardown checks; written by the media runner.
    pub active_priority_item: Arc<Mutex<Option<QueueItem>>>,
    pub track_handle: Arc<Mutex<Option<TrackHandle>>>,
    pub feeder_cancel: Arc<Mutex<Option<CancellationToken>>>,
    pub feeder_paused: Arc<AtomicBool>,
    pub dj: Arc<DJAnnouncer>,
    pub announce_enabled: Arc<AtomicBool>,
}

/// The actor's mailbox handle. Cheap to clone; the typed helpers build the
/// `Input`, send it, and await the oneshot reply.
#[derive(Clone)]
pub struct PlayerHandle(mpsc::UnboundedSender<Input>);

impl PlayerHandle {
    /// Send a reply-less input (transport events, media/voice reports,
    /// timer ticks). Dropped silently if the actor is gone.
    pub fn send(&self, input: Input) {
        let _ = self.0.send(input);
    }

    async fn request(&self, make: impl FnOnce(oneshot::Sender<String>) -> Input) -> String {
        let (tx, rx) = oneshot::channel();
        if self.0.send(make(tx)).is_err() {
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

    pub async fn previous(&self) -> String {
        self.request(|reply| Input::Previous { reply }).await
    }

    /// What's playing, as the core sees it (`/np`).
    pub async fn query(&self) -> String {
        self.request(|reply| Input::Query { reply }).await
    }
}

/// Spawn the player actor and return its handle. Called once, when the bot
/// is built; the actor lives for the process (lifecycle A).
pub fn spawn(deps: PlayerDeps) -> PlayerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let actor = Actor {
        deps,
        tx: tx.clone(),
        state: PlayerState::new(),
        pending_gate: None,
    };
    tokio::spawn(actor.run(rx));
    PlayerHandle(tx)
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
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>) {
        while let Some(input) = rx.recv().await {
            // Shell-side bridging that must happen on receipt, before the
            // core's own handling.
            match &input {
                Input::Transport { ev: TransportEvent::Paused { .. }, .. } => {
                    if let Some(gate) = self.pending_gate.take() {
                        gate.notify_one();
                    }
                }
                Input::Stop { .. } => {
                    // C3: /stop clears the *shared* arming slot too (the
                    // core only clears its own mirror) — same contract as
                    // the old handle_stop. Gone in C5.
                    *self.deps.armed_spotify.lock() = None;
                }
                _ => {}
            }

            // C3: run the core against the shared queue — swap it into the
            // state around the (synchronous) step, under the queue lock.
            let effects = {
                let mut shared = self.deps.priority_queue.lock();
                std::mem::swap(&mut *shared, &mut self.state.queue);
                let effects = step(&mut self.state, input, Instant::now());
                std::mem::swap(&mut *shared, &mut self.state.queue);
                effects
            };

            // A `StartMedia` in this batch: remember whose item it is (so a
            // cold-start voice join follows the requester) and mark the item
            // active before any effect runs — the presence loop must already
            // see it when the paired `Spirc(Pause)`'s echo comes back, or it
            // pauses the shared bridge-reader track under the new item.
            let mut join_hint = None;
            for effect in &effects {
                if let Effect::StartMedia { item, .. } = effect {
                    join_hint = Some(item.queued_by_id);
                    *self.deps.active_priority_item.lock() = Some(item.clone());
                }
            }

            for effect in effects {
                self.run_effect(effect, join_hint);
            }
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
                *self.deps.feeder_cancel.lock() = Some(token.clone());
                self.deps.feeder_paused.store(false, Ordering::Relaxed);

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

                let ctx = RunnerCtx {
                    bridge: self.deps.bridge.clone(),
                    track_handle: self.deps.track_handle.clone(),
                    active_priority_item: self.deps.active_priority_item.clone(),
                    feeder_paused: self.deps.feeder_paused.clone(),
                    dj: self.deps.dj.clone(),
                    announce_enabled: self.deps.announce_enabled.clone(),
                    ui_send: self.deps.ui_send.clone(),
                    notice_tx: self.deps.notice_tx.clone(),
                    input_tx: self.tx.clone(),
                };
                tokio::spawn(media_runner(ctx, item, epoch, token, gate_notify));
            }

            Effect::CancelMedia => {
                let token = { self.deps.feeder_cancel.lock().clone() };
                if let Some(token) = token {
                    token.cancel();
                }
            }

            Effect::ClearBridge => self.deps.bridge.clear(),

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

            Effect::LeaveVoice => {
                // No producer in C3 (teardown still leaves voice from the
                // Handler); wired when the core owns voice release.
                tracing::debug!("LeaveVoice effect dropped (not wired in C3)");
            }

            Effect::TrackHandle(cmd) => {
                let paused = matches!(cmd, TrackHandleCmd::Pause);
                // Pause the feeder too: the songbird pause freezes output
                // instantly, and the flag stops the download side from
                // racing ahead more than the bridge can hold.
                self.deps.feeder_paused.store(paused, Ordering::Relaxed);
                let handle = { self.deps.track_handle.lock().clone() };
                if let Some(handle) = handle {
                    let _ = if paused { handle.pause() } else { handle.play() };
                }
                (self.deps.ui_send)(UiEvent::Buttons { paused });
            }

            Effect::Ui(msg) => match msg {
                CoreUiMsg::NowPlayingMedia { item } => {
                    (self.deps.ui_send)(UiEvent::NowPlayingMedia { item });
                }
                CoreUiMsg::NowPlayingSpotify { .. } => {
                    // C3: the presence loop still posts Spotify cards from
                    // its own `Playing` handling; posting here too would
                    // double them. C5 hands the card trigger to the core.
                    tracing::debug!("NowPlayingSpotify suppressed (presence loop owns Spotify cards until C5)");
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
                // C3: `Playing` is suppressed — the shim already feeds the
                // presence loop the same event, and its `Playing` handler
                // still owns queue-pop/arm bookkeeping, which must run
                // exactly once. Idle/Paused are idempotent there.
                let update = match state {
                    PresenceState::Idle => Some(PresenceUpdate::Idle),
                    PresenceState::Playing { .. } => None,
                    PresenceState::Paused { uri, meta } => Some(PresenceUpdate::Paused {
                        title: meta.title,
                        artist: meta.artist,
                        track_id: uri.to_id(),
                    }),
                };
                if let Some(update) = update {
                    let _ = self.deps.presence_tx.send(update);
                }
            }

            Effect::Announce(_) => {
                // C3: Spotify-track announcements still come from the
                // presence loop; forwarding these would announce twice.
                // Media-item announcements live in the runner, not here.
                tracing::debug!("Announce suppressed (presence loop owns Spotify announces until C5)");
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
        match cmd {
            SpircCmd::Pause => {
                let _ = tx.send(SpircCommand::Pause);
            }
            SpircCmd::Play => {
                let _ = tx.send(SpircCommand::Play);
            }
            SpircCmd::Next => {
                let _ = tx.send(SpircCommand::Next);
            }
            SpircCmd::Previous => {
                let _ = tx.send(SpircCommand::Previous);
            }
            SpircCmd::AddToQueue(uri) => {
                // C3 arming bridge: join the presence loop's critical
                // section on the shared slot — whoever holds `armed_spotify`
                // first arms, so an armed track is never `AddToQueue`'d
                // twice. Mirrors `try_arm_first_spotify`: the slot is taken
                // only when the send goes through.
                let mut armed = self.deps.armed_spotify.lock();
                if armed.is_some() {
                    tracing::debug!("arm skipped (presence loop already armed a track)");
                    return;
                }
                if tx.send(SpircCommand::AddToQueue(uri.clone())).is_ok() {
                    *armed = Some(uri);
                }
            }
            SpircCmd::Load(uri) => {
                let _ = tx.send(SpircCommand::Load(uri));
            }
            SpircCmd::ActivateDevice => {
                let _ = tx.send(SpircCommand::ActivateDevice);
            }
            SpircCmd::Transfer => {
                let _ = tx.send(SpircCommand::Transfer);
            }
        }
    }
}

/// Everything one media runner needs, cloned out of the actor's deps.
struct RunnerCtx {
    bridge: Arc<AudioBridge>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
    active_priority_item: Arc<Mutex<Option<QueueItem>>>,
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

    *ctx.active_priority_item.lock() = None;

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
