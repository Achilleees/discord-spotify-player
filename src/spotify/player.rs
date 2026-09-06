use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::player::state::{TrackMeta, TransportEvent};
use crate::spotify::sink::{DiscordSink, DspConfig};
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::authentication::Credentials;
use librespot_core::config::{DeviceType, SessionConfig};
use librespot_core::session::Session;
use librespot_core::SpotifyUri;
use librespot_metadata::audio::item::{AudioItem, UniqueFields};
use librespot_metadata::image::Images;
use librespot_metadata::{Artist, Episode, Metadata, Track};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

const DEVICE_ID_FILE: &str = "device_id";
const MAX_FAST_RECONNECTS: usize = 5;
const MIN_STABLE_SESSION_SECS: u64 = 60;

/// Commands the Discord layer (queue drains, /skip, /stop, /np) sends to the
/// active Spirc instance. Not `Debug`: `Lookup`'s reply channel is a
/// `oneshot::Sender`, which isn't `Debug`.
pub enum SpircCommand {
    /// Tell Spotify this device is going away (so clients drop it from the
    /// device list at once) and end the session task.
    Shutdown,
    Pause,
    Play,
    Next,
    Previous,
    AddToQueue(SpotifyUri),
    /// Start playing this track now, replacing the current context.
    Load(SpotifyUri),
    /// Resolve a track's title/artist/art through the live session; replies
    /// `None` if unavailable.
    Lookup(SpotifyUri, tokio::sync::oneshot::Sender<Option<TrackLookup>>),
    /// Claim the active-device slot (F15: explicit only — never sent on a
    /// bare connect). A no-op on the Spotify side when this device is
    /// already active.
    ActivateDevice,
    /// `Spirc::transfer(None)`: restore context, position, pause state and
    /// the queue after a reconnect, claiming the active-device slot in the
    /// same call instead of a bare activate.
    Transfer,
    /// Release the active-device slot, pausing as it goes. The device stays
    /// in the Connect list; librespot ignores this while already inactive.
    Disconnect,
    /// Reopen a context at one of its tracks, restoring the playlist rather
    /// than replacing it the way `Load` does.
    LoadContext {
        context_uri: String,
        track_uri: SpotifyUri,
        /// Shuffle, repeat-context, repeat-track — or `None` to leave
        /// librespot's own settings untouched, which is what happens until
        /// Spotify has actually reported the DJ's.
        options: Option<(bool, bool, bool)>,
    },
}

/// Track metadata resolved through the live librespot session, for
/// describing a track (e.g. `/np`) without calling any Web API.
#[derive(Debug, Clone)]
pub struct TrackLookup {
    pub title: String,
    pub artist: String,
    pub album_art_url: Option<String>,
}

/// Info about the current track, kept in the event loop so a `Playing`
/// event (which only carries a bare `track_id`) can carry a `TrackMeta`
/// without calling any Web API. Filled by `TrackChanged`, consumed by the
/// next matching `Playing`.
struct CurrentTrack {
    track_id: SpotifyUri,
    title: String,
    artist: String,
    album_art_url: Option<String>,
}

/// Joins artist names in Spotify's own display order, dropping repeats.
/// Spotify's catalogue lists an artist twice on some tracks (a remixer
/// credited as both artist and remixer), which rendered as "Oliver Tree,
/// Georgie Riot, Georgie Riot" on the card while the enqueue-time lookup
/// showed it once.
fn join_unique(names: impl IntoIterator<Item = String>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for name in names {
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen.join(", ")
}

fn track_info_from_audio_item(audio_item: &AudioItem) -> CurrentTrack {
    let artist = match &audio_item.unique_fields {
        UniqueFields::Track { artists, .. } => {
            join_unique(artists.iter().map(|a| a.name.clone()))
        }
        UniqueFields::Local { artists, .. } => artists.clone().unwrap_or_default(),
        UniqueFields::Episode { .. } => String::new(),
    };
    let album_art_url = audio_item
        .covers
        .iter()
        .max_by_key(|c| c.width)
        .map(|c| c.url.clone());

    CurrentTrack {
        track_id: audio_item.track_id.clone(),
        title: audio_item.name.clone(),
        artist,
        album_art_url,
    }
}

/// Aborts the wrapped task when dropped. Dropping a bare `JoinHandle` only
/// detaches the task; this guard ensures the event loop dies with its session.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Await a command from an optional receiver; parks forever when there is no
/// receiver, so it can sit in a `select!` without firing.
async fn recv_cmd(rx: &mut Option<mpsc::UnboundedReceiver<SpircCommand>>) -> Option<SpircCommand> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

pub struct SpotifyPlayer;

impl SpotifyPlayer {
    fn cache_dir() -> PathBuf {
        crate::runtime::paths().spotify_cache.clone()
    }

    fn resolve_device_id(config: &Config) -> String {
        Self::resolve_device_id_at(config.device_id.as_deref(), &Self::cache_dir())
    }

    fn resolve_device_id_at(explicit: Option<&str>, cache_dir: &std::path::Path) -> String {
        if let Some(id) = explicit {
            return id.to_string();
        }

        let path = cache_dir.join(DEVICE_ID_FILE);
        if let Ok(id) = std::fs::read_to_string(&path) {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        let id = hex::encode(rand::random::<[u8; 20]>());
        if std::fs::create_dir_all(cache_dir).is_ok() {
            let _ = std::fs::write(&path, id.as_bytes());
        }
        id
    }

    // No librespot Cache: it only persisted reusable credentials that nothing
    // ever read back — every (re)connect authenticates with the OAuth access
    // token. Only the device_id file above needs the cache directory.
    fn create_session(device_id: &str) -> Session {
        let session_config = SessionConfig {
            device_id: device_id.to_string(),
            ..SessionConfig::default()
        };
        Session::new(session_config, None)
    }

    fn connect_config(device_name: &str) -> ConnectConfig {
        ConnectConfig {
            name: device_name.to_string(),
            device_type: DeviceType::Computer,
            is_group: false,
            // 70% of full scale. This is not only what Spotify clients
            // display: `Spirc::new` feeds it straight to the soft mixer
            // (connect/src/spirc.rs @1599145, "we just want to set the mixer
            // to the correct volume"), and the default `VolumeCtrl::Log(60)`
            // maps 70% to an amplitude of about 0.1259 — -18 dB of real
            // attenuation on everything Spotify plays. The media path
            // (yt-dlp, files) has no mixer, so it arrives 18 dB hotter; see
            // the loudness note in docs/architecture.md.
            initial_volume: 45875,
            disable_volume: false,
            volume_steps: 64,
            // SetQueue reports Spotify's own queue on every mutation; kept
            // on so it's available for arm confirmation.
            emit_set_queue_events: true,
        }
    }

    /// All artists' names, joined the way Spotify itself displays multiple
    /// artists. Used by `lookup_track`.
    fn join_artist_names<'a>(artists: impl IntoIterator<Item = &'a Artist>) -> String {
        let joined = join_unique(artists.into_iter().map(|a| a.name.clone()));
        if joined.is_empty() {
            "Unknown artist".to_string()
        } else {
            joined
        }
    }

    /// Largest-resolution cover art URL for a track/album's cover set.
    /// Replicates the `{file_id}` template substitution
    /// `librespot_metadata::audio::item::AudioItem::get_file` does
    /// internally — that path isn't used here, so it has to be redone.
    fn largest_cover_url(session: &Session, covers: &Images) -> Option<String> {
        let template = session
            .get_user_attribute("image-url")
            .unwrap_or_else(|| String::from("https://i.scdn.co/image/{file_id}"));
        covers
            .iter()
            .max_by_key(|c| c.width)
            .map(|c| template.replace("{file_id}", &c.id.to_string()))
    }

    /// Resolves title/artist/art for a track or episode through the live
    /// session, backing `SpircCommand::Lookup`. No Web API call.
    async fn lookup_track(session: &Session, uri: &SpotifyUri) -> Option<TrackLookup> {
        match uri {
            SpotifyUri::Track { .. } => {
                let track = Track::get(session, uri).await.ok()?;
                let artist = Self::join_artist_names(track.artists.iter());
                let album_art_url = Self::largest_cover_url(session, &track.album.covers);
                Some(TrackLookup {
                    title: track.name,
                    artist,
                    album_art_url,
                })
            }
            SpotifyUri::Episode { .. } => {
                let episode = Episode::get(session, uri).await.ok()?;
                Some(TrackLookup {
                    title: episode.name,
                    artist: "Podcast".to_string(),
                    album_art_url: None,
                })
            }
            _ => None,
        }
    }

    fn spawn_event_loop(
        rx: tokio::sync::mpsc::UnboundedReceiver<PlayerEvent>,
        transport_tx: mpsc::UnboundedSender<TransportEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut rx = rx;
            // Set by TrackChanged, consumed by the next Playing for that
            // same track_id. Nothing here ever calls api.spotify.com.
            let mut current: Option<CurrentTrack> = None;
            let mut last_sent_track: Option<SpotifyUri> = None;
            // Spotify reports shuffle and repeat in separate events, each
            // carrying only its own half, and only when something *changes* —
            // so a session where the DJ touches neither reports nothing at
            // all. Each half stays absent until it is actually observed, and
            // nothing is forwarded until both are: a picture completed with
            // invented defaults gets handed back on the next context jump,
            // which is how a back-jump could turn the DJ's shuffle off.
            let mut last_shuffle: Option<bool> = None;
            let mut last_repeat: Option<(bool, bool)> = None;

            while let Some(event) = rx.recv().await {
                match event {
                    PlayerEvent::TrackChanged { audio_item } => {
                        let info = track_info_from_audio_item(&audio_item);
                        let uri = info.track_id.clone();
                        let meta = TrackMeta {
                            title: info.title.clone(),
                            artist: info.artist.clone(),
                            album_art_url: info.album_art_url.clone(),
                        };
                        current = Some(info);
                        let _ = transport_tx.send(TransportEvent::TrackChanged { uri, meta });
                    }
                    PlayerEvent::Playing { track_id, .. } => {
                        let is_new_track = last_sent_track.as_ref() != Some(&track_id);
                        let meta = match &current {
                            Some(c) if c.track_id == track_id => Some(TrackMeta {
                                title: c.title.clone(),
                                artist: c.artist.clone(),
                                album_art_url: c.album_art_url.clone(),
                            }),
                            // No TrackChanged remembered for this id (or it
                            // doesn't match) — the player core falls back to
                            // its own last-heard/queue metadata for this uri.
                            _ => None,
                        };

                        if is_new_track {
                            // Console announcement happens downstream, once
                            // the track is actually heard (a Playing under an
                            // active media item is paused straight back
                            // down).
                            tracing::debug!(track_id = %track_id, has_meta = meta.is_some(), "spotify playing event");
                        }
                        last_sent_track = Some(track_id.clone());
                        let _ = transport_tx.send(TransportEvent::Playing { uri: track_id, meta });
                    }
                    PlayerEvent::Paused { track_id, .. } => {
                        let _ = transport_tx.send(TransportEvent::Paused { uri: track_id });
                        tracing::debug!("playback paused");
                    }
                    PlayerEvent::Stopped { .. } => {
                        let _ = transport_tx.send(TransportEvent::Stopped);
                        tracing::debug!("playback stopped");
                    }
                    PlayerEvent::Loading { .. } => {
                        tracing::debug!("loading track");
                    }
                    PlayerEvent::EndOfTrack { .. } => {
                        last_sent_track = None;
                        let _ = transport_tx.send(TransportEvent::EndOfTrack);
                    }
                    PlayerEvent::Unavailable { track_id, .. } => {
                        let _ = transport_tx.send(TransportEvent::Unavailable { uri: track_id });
                        println!("Track unavailable");
                    }
                    PlayerEvent::SetQueue {
                        current_track,
                        next_tracks,
                        context_uri,
                        ..
                    } => {
                        let current: Option<SpotifyUri> =
                            current_track.and_then(|t| SpotifyUri::from_uri(&t.uri).ok());
                        // Only "queue"-provider entries are ours (the ones
                        // AddToQueue creates) — context/autoplay tracks in
                        // next_tracks aren't arm confirmations.
                        let queued: Vec<SpotifyUri> = next_tracks
                            .into_iter()
                            .filter(|t| t.provider == "queue")
                            .filter_map(|t| SpotifyUri::from_uri(&t.uri).ok())
                            .collect();
                        // Spotify sends an empty string when the playback
                        // has no named context (a bare track, autoplay
                        // before it resolves); that is absence, not a name.
                        let context_uri = Some(context_uri).filter(|c| !c.is_empty());
                        let _ = transport_tx.send(TransportEvent::SetQueue {
                            current,
                            queued,
                            context_uri,
                        });
                    }
                    PlayerEvent::ShuffleChanged { shuffle } => {
                        last_shuffle = Some(shuffle);
                        if let Some((repeat_context, repeat_track)) = last_repeat {
                            let _ = transport_tx.send(TransportEvent::OptionsChanged {
                                shuffle,
                                repeat_context,
                                repeat_track,
                            });
                        }
                    }
                    PlayerEvent::RepeatChanged { context, track } => {
                        last_repeat = Some((context, track));
                        if let Some(shuffle) = last_shuffle {
                            let _ = transport_tx.send(TransportEvent::OptionsChanged {
                                shuffle,
                                repeat_context: context,
                                repeat_track: track,
                            });
                        }
                    }
                    PlayerEvent::SessionConnected { .. } => {
                        let _ = transport_tx.send(TransportEvent::SessionConnected);
                    }
                    PlayerEvent::SessionDisconnected { .. } => {
                        let _ = transport_tx.send(TransportEvent::SessionDisconnected);
                    }
                    _ => {}
                }
            }
        })
    }

    /// Run Spotify Connect using an OAuth access token (no discovery).
    ///
    /// The command receiver is borrowed, not consumed: it stays alive across
    /// reconnect iterations here and returns to the caller intact, so a
    /// restarted session keeps a live Pause/Play channel.
    pub async fn run_with_token(
        config: &Config,
        bridge: Arc<AudioBridge>,
        transport_tx: mpsc::UnboundedSender<TransportEvent>,
        access_token: String,
        spirc_cmd_rx: &mut Option<mpsc::UnboundedReceiver<SpircCommand>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let device_id = Self::resolve_device_id(config);
        let device_name = config.device_name.clone();
        let mut reconnects: usize = 0;

        let credentials = Credentials::with_access_token(access_token.clone());

        loop {
            let session = Self::create_session(&device_id);
            let connect_config = Self::connect_config(&device_name);
            let mixer: Arc<dyn Mixer> = Arc::new(
                SoftMixer::open(MixerConfig::default()).expect("Failed to create audio mixer"),
            );

            let player_config = PlayerConfig {
                bitrate: librespot_playback::config::Bitrate::Bitrate320,
                ..Default::default()
            };

            let bridge_clone = bridge.clone();
            let dsp_config = DspConfig::new(
                config.preamp_db,
                config.bass_boost_db,
                config.treble_boost_db,
            );
            let player = Player::new(player_config, session.clone(), mixer.get_soft_volume(), {
                move || Box::new(DiscordSink::new(bridge_clone.clone(), dsp_config))
            });
            let rx = player.get_player_event_channel();

            // Event loop is guarded so it is aborted when this future is
            // dropped (logout/takeover) or when the loop reconnects, rather
            // than left detached to keep forwarding a dead generation's
            // transport events after a new one has already taken over. It
            // never touches the bridge itself — clearing it is the player
            // core's call now (`Effect::ClearBridge`, gated on whether
            // Spotify holds the turn), not this raw event forwarder's.
            let _event_guard = AbortOnDrop(Self::spawn_event_loop(rx, transport_tx.clone()));

            tracing::info!(device_id = %device_id, device_name = %device_name, "calling Spirc::new");
            let (spirc, spirc_task) = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                Spirc::new(connect_config, session.clone(), credentials.clone(), player, mixer),
            )
            .await
            .map_err(|_| {
                tracing::error!("Spirc::new timed out after 15s");
                Box::<dyn std::error::Error + Send + Sync>::from("spirc connect timeout")
            })?
            .map_err(|e| {
                tracing::error!(error = ?e, "OAuth session connect failed");
                e
            })?;

            // No unconditional activate() here (F15): claiming the active
            // Connect device on every connect steals it from whatever's
            // already playing on the DJ's phone. Activation is explicit now —
            // `SpircCommand::ActivateDevice`, sent only on a human play or
            // takeover — and a reconnect that owes a restore sends
            // `SpircCommand::Transfer` instead, never a bare activate.
            tracing::info!("spotify connect established (oauth)");
            let session_start = std::time::Instant::now();

            // Run the spirc task inline (pinned, not detached) so it is
            // cancelled with this future, and so its completion — the signal
            // that the Connect session died — actually breaks us out to the
            // reconnect path instead of parking forever on cmd_rx.recv().
            tokio::pin!(spirc_task);
            loop {
                tokio::select! {
                    _ = &mut spirc_task => {
                        tracing::info!("spirc task ended (session closed)");
                        break;
                    }
                    maybe_cmd = recv_cmd(spirc_cmd_rx) => {
                        match maybe_cmd {
                            Some(SpircCommand::Shutdown) => {
                                let _ = spirc.shutdown();
                                tracing::info!("spirc shut down (session stop)");
                                return Ok(());
                            }
                            Some(SpircCommand::Pause) => { let _ = spirc.pause(); }
                            Some(SpircCommand::Play)  => { let _ = spirc.play();  }
                            Some(SpircCommand::Next) => {
                                if let Err(e) = spirc.next() {
                                    tracing::warn!(error = ?e, "spirc next failed");
                                }
                            }
                            Some(SpircCommand::Previous) => {
                                if let Err(e) = spirc.prev() {
                                    tracing::warn!(error = ?e, "spirc previous failed");
                                }
                            }
                            Some(SpircCommand::AddToQueue(uri)) => {
                                if let Err(e) = spirc.add_to_queue(uri) {
                                    tracing::warn!(error = ?e, "spirc add_to_queue failed");
                                }
                            }
                            Some(SpircCommand::Load(uri)) => {
                                // Loading is ignored unless this device is
                                // the active Connect device; claim it first
                                // (a no-op when already active).
                                if let Err(e) = spirc.activate() {
                                    tracing::warn!(error = ?e, "spirc activate failed");
                                }
                                let req = LoadRequest::from_tracks(
                                    vec![uri.to_uri()],
                                    LoadRequestOptions {
                                        start_playing: true,
                                        ..Default::default()
                                    },
                                );
                                if let Err(e) = spirc.load(req) {
                                    tracing::warn!(error = ?e, "spirc load failed");
                                }
                            }
                            Some(SpircCommand::ActivateDevice) => {
                                if let Err(e) = spirc.activate() {
                                    tracing::warn!(error = ?e, "spirc activate failed");
                                }
                            }
                            Some(SpircCommand::Transfer) => {
                                if let Err(e) = spirc.transfer(None) {
                                    tracing::warn!(error = ?e, "spirc transfer failed");
                                }
                            }
                            Some(SpircCommand::LoadContext {
                                context_uri,
                                track_uri,
                                options,
                            }) => {
                                // Same active-device requirement as Load.
                                if let Err(e) = spirc.activate() {
                                    tracing::warn!(error = ?e, "spirc activate failed");
                                }
                                let req = LoadRequest::from_context_uri(
                                    context_uri,
                                    LoadRequestOptions {
                                        start_playing: true,
                                        playing_track: Some(PlayingTrack::Uri(
                                            track_uri.to_uri(),
                                        )),
                                        // Without these librespot resets
                                        // shuffle/repeat on every load, which
                                        // would quietly turn the DJ's shuffle
                                        // off on a back-jump. Sending values
                                        // it never reported would do the same
                                        // thing, so absence stays absence.
                                        context_options: options.map(
                                            |(shuffle, repeat, repeat_track)| {
                                                LoadContextOptions::Options(Options {
                                                    shuffle,
                                                    repeat,
                                                    repeat_track,
                                                })
                                            },
                                        ),
                                        ..Default::default()
                                    },
                                );
                                if let Err(e) = spirc.load(req) {
                                    tracing::warn!(error = ?e, "spirc context load failed");
                                }
                            }
                            Some(SpircCommand::Disconnect) => {
                                // Always with the pause: the non-pausing form
                                // leaves the player decoding after the bot
                                // has gone, and its next event would re-take
                                // the device.
                                if let Err(e) = spirc.disconnect(true) {
                                    tracing::warn!(error = ?e, "spirc disconnect failed");
                                }
                            }
                            Some(SpircCommand::Lookup(uri, reply)) => {
                                let result = Self::lookup_track(&session, &uri).await;
                                let _ = reply.send(result);
                            }
                            None => *spirc_cmd_rx = None, // all senders dropped; poll the task only
                        }
                    }
                }
            }

            let _ = spirc.shutdown();
            drop(spirc);

            let session_duration = session_start.elapsed();
            if session_duration >= std::time::Duration::from_secs(MIN_STABLE_SESSION_SECS) {
                reconnects = 0;
            }

            if reconnects < MAX_FAST_RECONNECTS {
                reconnects += 1;
                let delay = std::time::Duration::from_secs(2u64.saturating_mul(reconnects as u64));
                tracing::info!(attempt = reconnects, delay = ?delay, "oauth session dropped, fast reconnect");
                tokio::time::sleep(delay).await;
                continue;
            }

            tracing::info!("max reconnects reached, returning for token refresh");
            let _ = transport_tx.send(TransportEvent::SessionDisconnected);
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::join_unique;

    #[test]
    fn repeated_artists_are_listed_once() {
        assert_eq!(
            join_unique(["Oliver Tree", "Georgie Riot", "Georgie Riot"].map(String::from)),
            "Oliver Tree, Georgie Riot"
        );
    }

    #[test]
    fn distinct_artists_keep_spotifys_order() {
        assert_eq!(
            join_unique(["Ed:it", "Pola & Bryson"].map(String::from)),
            "Ed:it, Pola & Bryson"
        );
    }

    #[test]
    fn no_artists_joins_to_nothing() {
        assert_eq!(join_unique(Vec::<String>::new()), "");
    }
}

#[cfg(test)]
mod device_identity_tests {
    use super::SpotifyPlayer;

    #[test]
    fn identity_is_stable_per_cache_and_separate_between_hosts() {
        let root = std::env::temp_dir().join(format!("bot-device-test-{}", uuid::Uuid::new_v4()));
        let spot = root.join("spotibot");
        let nob = root.join("nob");
        let first = SpotifyPlayer::resolve_device_id_at(None, &spot);
        assert_eq!(first.len(), 40);
        assert_eq!(first, SpotifyPlayer::resolve_device_id_at(None, &spot));
        assert_ne!(first, SpotifyPlayer::resolve_device_id_at(None, &nob));
        assert_eq!(SpotifyPlayer::resolve_device_id_at(Some("explicit-test-id"), &spot), "explicit-test-id");
        assert_eq!(first, SpotifyPlayer::resolve_device_id_at(None, &spot));
        std::fs::remove_dir_all(root).unwrap();
    }
}
