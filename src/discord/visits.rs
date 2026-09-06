//! Nob clips mix into an existing same-room music connection, or make a
//! temporary visit while idle. Overlay cleanup never owns music or voice.

use super::{
    bot::{consume_deliberate_leave, CursorSource, Handler},
    voice_owner::{OverlayLease, VoiceLease, VoiceOwner},
};
use crate::{
    audio_bridge::{AudioBridge, OverlayError, OverlayHandle, OverlayStatus},
    player::state::NowPlaying,
    routing::VoiceActivity,
    runtime::Profile,
    soundboard::Catalogue,
};
use serenity::all::{ChannelId, ChannelType, Context, EditVoiceState, GuildId, UserId};
use songbird::{
    events::EventData,
    input::RawAdapter,
    tracks::{Track, TrackHandle},
    Event, EventContext, EventHandler, TrackEvent,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::watch;

const JOIN_LIMIT: Duration = Duration::from_secs(12);
const VISIT_LIMIT: Duration = Duration::from_secs(40);
const LEAVE_LIMIT: Duration = Duration::from_secs(5);
const ARRIVAL_PAUSE: Duration = Duration::from_millis(1_500);
const DEPARTURE_PAUSE: Duration = Duration::from_secs(2);
const CANCELLED: &str = "Visit ended early: your room changed or music took over.";

/// Check the requester's actual listening room, independently of the bot's
/// room. Call immediately before reserving, after decode and after joining.
fn audience(ctx: &Context, guild: GuildId, user: UserId, room: u64) -> bool {
    let Some(guild) = guild.to_guild_cached(ctx) else {
        return false;
    };
    let channel = guild
        .voice_states
        .get(&user)
        .and_then(|voice| voice.channel_id);
    room != 0
        && channel.map(|id| id.get()) == Some(room)
        && guild
            .afk_metadata
            .as_ref()
            .is_none_or(|afk| Some(afk.afk_channel_id) != channel)
}

impl Handler {
    pub(super) async fn play_soundboard_clip(
        &self,
        ctx: &Context,
        user: UserId,
        clip_id: &str,
        room: u64,
        generation: u64,
    ) -> String {
        if self.config.profile != Profile::Nob {
            return "The soundboard belongs to nob.".into();
        }
        if !audience(ctx, self.guild_id, user, room) {
            return "Join a voice channel and refresh the soundboard.".into();
        }
        if self.voice_owner.activity() == Some(VoiceActivity::Music) {
            if self
                .voice_channels(ctx, user)
                .0
                .map(|channel| channel.get())
                != Some(room)
            {
                return "Join nob's voice room and refresh the soundboard to play over his music."
                    .into();
            }
            let Some(lease) = self.voice_owner.claim_overlay(room, user.get(), generation) else {
                return "nob is connecting, another sound is playing, or this menu is out of date. Refresh and try again.".into();
            };
            let overlay = MusicOverlay {
                ctx: ctx.clone(),
                guild: self.guild_id,
                user,
                owner: self.voice_owner.clone(),
                lease,
                bridge: self.bridge.clone(),
                epoch: self.bridge.overlay_epoch(),
                volume: self.config.soundboard_volume,
                audio: None,
            };
            let catalogue = self.soundboard.clone();
            let clip_id = clip_id.to_owned();
            return match tokio::spawn(async move { overlay.run(catalogue, clip_id).await }).await {
                Ok(reply) => reply,
                Err(error) => {
                    tracing::warn!(?error, "soundboard overlay task failed");
                    "The sound couldn't finish.".into()
                }
            };
        }
        let snapshot = self.player.query().await;
        if !matches!(snapshot.now, NowPlaying::Nothing)
            || self.voice_channels(ctx, user).0.is_some()
        {
            return "nob is busy with another voice session. Try again when he's free.".into();
        }
        if !audience(ctx, self.guild_id, user, room) {
            return "Your voice room changed. Refresh the soundboard.".into();
        }
        let Some(lease) = self.voice_owner.claim_visit(room, user.get(), generation) else {
            return "nob is busy or this menu is out of date. Refresh and try again.".into();
        };
        let visit = Visit {
            ctx: ctx.clone(),
            guild: self.guild_id,
            user,
            owner: self.voice_owner.clone(),
            lease,
            leaving: self.leaving_voice.clone(),
            volume: self.config.soundboard_volume,
            track: None,
            cleaned: false,
        };
        let catalogue = self.soundboard.clone();
        let clip_id = clip_id.to_owned();
        // Accepted visits outlive a lost interaction response. The task's own
        // deadline and cleanup still run if the waiting handler is dropped.
        match tokio::spawn(async move { visit.run(catalogue, clip_id).await }).await {
            Ok(reply) => reply,
            Err(error) => {
                tracing::warn!(?error, "soundboard visit task failed");
                "The soundboard visit couldn't finish.".into()
            }
        }
    }
}

/// This guard owns only one overlay and its requester reservation. Dropping
/// it after completion, failure or panic cannot stop music or leave voice.
struct MusicOverlay {
    ctx: Context,
    guild: GuildId,
    user: UserId,
    owner: Arc<VoiceOwner>,
    lease: OverlayLease,
    bridge: Arc<AudioBridge>,
    epoch: u64,
    volume: f32,
    audio: Option<OverlayHandle>,
}

impl MusicOverlay {
    async fn run(mut self, catalogue: Arc<Catalogue>, clip_id: String) -> String {
        let cancel = self.lease.cancelled.clone();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("Sound cancelled: the voice room or session changed.".into()),
            result = tokio::time::timeout(VISIT_LIMIT, self.play(&catalogue, &clip_id)) =>
                result.unwrap_or_else(|_| Err("The sound timed out.".into())),
        };
        match result {
            Ok(()) => "Sound played.".into(),
            Err(message) => message,
        }
    }

    async fn play(&mut self, catalogue: &Catalogue, clip_id: &str) -> Result<(), String> {
        let bytes = catalogue.decode(clip_id).await?;
        let duration = Duration::from_secs_f64(bytes.len() as f64 / (44_100.0 * 2.0 * 4.0));
        let samples = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| f32::from_le_bytes(*sample))
            .collect();
        let manager = songbird::get(&self.ctx)
            .await
            .ok_or("Voice is unavailable right now.")?;
        let call = manager
            .get(self.guild)
            .ok_or("nob's voice session ended.")?;
        let transition = self.owner.transitions.lock().await;
        let connected_room = call
            .lock()
            .await
            .current_channel()
            .map(|channel| channel.0.get());
        if !self.owner.overlay_current(&self.lease)
            || !audience(&self.ctx, self.guild, self.user, self.lease.channel)
            || connected_room != Some(self.lease.channel)
        {
            return Err(
                "Your voice room or nob's connection changed. Refresh the soundboard.".into(),
            );
        }
        let audio = self.owner.with_overlay_current(&self.lease, || {
            self.bridge.start_overlay(self.epoch, samples, self.volume)
        });
        self.audio = Some(
            audio
                .ok_or("Sound cancelled: the voice room or session changed.")?
                .map_err(|error| match error {
                    OverlayError::Busy => "Another sound or DJ line is playing. Try again shortly.",
                    OverlayError::Stale => {
                        "Music changed while preparing the sound. Refresh and try again."
                    }
                    _ => "This sound couldn't be played. Try another clip.",
                })?,
        );
        drop(transition);
        // Completion is observed from actual bridge consumption. A timer is
        // only a failure bound, never evidence that a clip was played.
        tokio::time::timeout(duration + Duration::from_secs(5), async {
            let mut tick = tokio::time::interval(Duration::from_millis(25));
            loop {
                match self.audio.as_ref().expect("installed overlay").status() {
                    OverlayStatus::Drained => return Ok(()),
                    OverlayStatus::Cancelled => {
                        return Err("Sound cancelled because the music session changed.".into())
                    }
                    OverlayStatus::Playing => tick.tick().await,
                };
            }
        })
        .await
        .map_err(|_| "The sound didn't finish in time.")?
    }
}

impl Drop for MusicOverlay {
    fn drop(&mut self) {
        if let Some(audio) = self.audio.take() {
            self.bridge.cancel_overlay(&audio);
        }
        self.owner.release_overlay(&self.lease);
    }
}

struct Finished {
    result: watch::Sender<Option<bool>>,
    success: bool,
}

#[serenity::async_trait]
impl EventHandler for Finished {
    async fn act(&self, _: &EventContext<'_>) -> Option<Event> {
        self.result.send_if_modified(|result| {
            if result.is_none() {
                *result = Some(self.success);
                true
            } else {
                false
            }
        });
        Some(Event::Cancel)
    }
}

struct Visit {
    ctx: Context,
    guild: GuildId,
    user: UserId,
    owner: Arc<VoiceOwner>,
    lease: VoiceLease,
    leaving: Arc<AtomicUsize>,
    volume: f32,
    track: Option<TrackHandle>,
    cleaned: bool,
}

impl Visit {
    async fn run(mut self, catalogue: Arc<Catalogue>, clip_id: String) -> String {
        let cancel = self.lease.cancelled.clone();
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(CANCELLED.to_string()),
            result = tokio::time::timeout(VISIT_LIMIT, self.play(&catalogue, &clip_id)) => result.unwrap_or_else(|_| Err("The soundboard visit timed out.".into())),
        };
        if let Some(track) = self.track.take() {
            let _ = track.stop();
        }
        let removed = cleanup(
            &self.ctx,
            self.guild,
            &self.owner,
            &self.lease,
            &self.leaving,
        )
        .await;
        self.cleaned = removed;
        if !removed {
            return "The clip stopped, but nob couldn't confirm leaving voice. Check his connection before retrying.".into();
        }
        match outcome {
            Ok(()) => "Sound played. See you next time!".into(),
            Err(message) => message,
        }
    }

    async fn play(&mut self, catalogue: &Catalogue, clip_id: &str) -> Result<(), String> {
        let bytes = catalogue.decode(clip_id).await?;
        let duration = Duration::from_secs_f64(bytes.len() as f64 / (44_100.0 * 2.0 * 4.0));
        let manager = songbird::get(&self.ctx)
            .await
            .ok_or("Voice is unavailable right now.")?;
        let transition = self.owner.transitions.lock().await;
        self.validate()?;
        let call = tokio::time::timeout(
            JOIN_LIMIT,
            manager.join(self.guild, ChannelId::new(self.lease.channel)),
        )
        .await
        .map_err(|_| "nob couldn't join voice in time.")?
        .map_err(|_| "nob couldn't join your voice channel. Check his voice permissions.")?;
        if !self.owner.mark_connected(&self.lease) {
            return Err(CANCELLED.into());
        }
        self.validate()?;
        {
            let mut call = call.lock().await;
            tokio::time::timeout(Duration::from_secs(3), call.deafen(true))
                .await
                .map_err(|_| "Voice setup timed out.")?
                .map_err(|_| "Voice setup failed.")?;
        }
        let stage = self.guild.to_guild_cached(&self.ctx).and_then(|guild| {
            guild
                .channels
                .get(&ChannelId::new(self.lease.channel))
                .filter(|ch| ch.kind == ChannelType::Stage)
                .cloned()
        });
        if let Some(room) = stage {
            tokio::time::timeout(
                Duration::from_secs(3),
                room.edit_own_voice_state(&self.ctx, EditVoiceState::new().suppress(false)),
            )
            .await
            .map_err(|_| "Stage voice setup timed out.")?
            .map_err(|_| "nob needs permission to speak on this stage.")?;
        }
        // Give listeners a beat after joining. The visit's outer cancellation
        // still wins immediately; never hold voice transitions while waiting.
        drop(transition);
        tokio::time::sleep(ARRIVAL_PAUSE).await;
        let transition = self.owner.transitions.lock().await;
        self.validate()?;
        let (result, mut finished) = watch::channel(None);
        let raw = RawAdapter::new(CursorSource(std::io::Cursor::new(bytes)), 44_100, 2);
        let mut track = Track::new(raw.into());
        track.volume = self.volume;
        for (event, success) in [(TrackEvent::End, true), (TrackEvent::Error, false)] {
            track.events.add_event(
                EventData::new(
                    Event::Track(event),
                    Finished {
                        result: result.clone(),
                        success,
                    },
                ),
                Duration::ZERO,
            );
        }
        let mut call = call.lock().await;
        self.track = self
            .owner
            .with_current(&self.lease, || call.play_only(track));
        if self.track.is_none() {
            return Err(CANCELLED.into());
        }
        drop(call);
        drop(transition);
        // End/error events are installed before playback, including tiny clips.
        // A duration timer is only a failure bound, never proof it was heard.
        let finished = tokio::time::timeout(duration + Duration::from_secs(5), async {
            loop {
                if let Some(success) = *finished.borrow_and_update() {
                    return success;
                }
                if finished.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .map_err(|_| "The sound didn't finish in time.")?;
        if !finished {
            return Err("The sound couldn't be played.".into());
        }
        // Only a successful clip gets a short goodbye pause. Cancellation or
        // music preemption during it goes straight to the fenced cleanup.
        tokio::time::sleep(DEPARTURE_PAUSE).await;
        self.validate()?;
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.owner.current(&self.lease)
            && audience(&self.ctx, self.guild, self.user, self.lease.channel)
        {
            Ok(())
        } else {
            Err(CANCELLED.into())
        }
    }
}

/// Stop even on task panic/cancellation, then retry fenced cleanup in the
/// runtime. No player/account teardown belongs to a temporary clip visit.
impl Drop for Visit {
    fn drop(&mut self) {
        if let Some(track) = self.track.take() {
            let _ = track.stop();
        }
        if self.cleaned || !self.owner.owns(&self.lease) {
            return;
        }
        self.lease.cancelled.cancel();
        let (ctx, guild, owner, lease, leaving) = (
            self.ctx.clone(),
            self.guild,
            self.owner.clone(),
            self.lease.clone(),
            self.leaving.clone(),
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // At most one retry task remains for the owned visit. A new
                // music claim ends it; a reconnect allows removal to succeed.
                while owner.owns(&lease) {
                    if cleanup(&ctx, guild, &owner, &lease, &leaving).await {
                        break;
                    }
                    tracing::debug!("soundboard voice cleanup deferred until connection recovers");
                    tokio::time::sleep(Duration::from_secs(15)).await;
                }
            });
        }
    }
}

/// An interrupted remove must not swallow a later genuine disconnect.
struct Departure<'a> {
    counter: &'a AtomicUsize,
    pending: bool,
}
impl<'a> Departure<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self {
            counter,
            pending: true,
        }
    }
    fn commit(&mut self) {
        self.pending = false;
    }
}
impl Drop for Departure<'_> {
    fn drop(&mut self) {
        if self.pending {
            consume_deliberate_leave(self.counter);
        }
    }
}

async fn cleanup(
    ctx: &Context,
    guild: GuildId,
    owner: &VoiceOwner,
    lease: &VoiceLease,
    leaving: &AtomicUsize,
) -> bool {
    if !owner.owns(lease) {
        return true;
    }
    lease.cancelled.cancel();
    let operation = async {
        let _transition = owner.transitions.lock().await;
        if !owner.owns(lease) {
            return true;
        }
        let Some(manager) = songbird::get(ctx).await else {
            owner.retire_if(lease);
            return true;
        };
        let Some(call) = manager.get(guild) else {
            owner.retire_if(lease);
            return true;
        };
        let connected = call.lock().await.current_channel().is_some();
        let mut departure = connected.then(|| Departure::new(leaving));
        if manager.remove(guild).await.is_err() {
            return false;
        }
        if let Some(departure) = &mut departure {
            departure.commit();
        }
        owner.retire_if(lease);
        true
    };
    tokio::time::timeout(LEAVE_LIMIT, operation)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn the_first_terminal_event_wins() {
        let (tx, rx) = watch::channel(None);
        let end = Finished {
            result: tx.clone(),
            success: true,
        };
        let error = Finished {
            result: tx,
            success: false,
        };
        let event = EventContext::Track(&[]);
        error.act(&event).await;
        end.act(&event).await;
        assert_eq!(*rx.borrow(), Some(false));
    }
    #[tokio::test]
    async fn cancelled_departure_rolls_back_only_its_arming() {
        let count = AtomicUsize::new(1);
        let removed = tokio::time::timeout(Duration::from_millis(5), async {
            let _arm = Departure::new(&count);
            std::future::pending::<()>().await;
        })
        .await;
        assert!(removed.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        {
            let mut arm = Departure::new(&count);
            arm.commit();
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(consume_deliberate_leave(&count));
        assert!(consume_deliberate_leave(&count));
        assert!(!consume_deliberate_leave(&count));
    }
}
