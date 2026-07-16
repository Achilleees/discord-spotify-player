use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::presence::PresenceUpdate;
use crate::spotify::sink::{DiscordSink, DspConfig};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::{DeviceType, SessionConfig};
use librespot_core::session::Session;
use librespot_core::SpotifyUri;
use librespot_metadata::{Episode, Metadata, Track};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

const CACHE_DIR: &str = ".spotify_cache";
const DEVICE_ID_FILE: &str = "device_id";
const MAX_CACHED_RECONNECTS: usize = 5;
const MIN_STABLE_SESSION_SECS: u64 = 60;

/// Commands that can be sent to the active Spirc instance from the priority queue manager.
pub enum SpircCommand {
    Pause,
    Play,
}

fn extract_track_id(uri: &SpotifyUri) -> String {
    uri.to_id().unwrap_or_default()
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
        PathBuf::from(CACHE_DIR)
    }

    fn device_id_path() -> PathBuf {
        Self::cache_dir().join(DEVICE_ID_FILE)
    }

    fn resolve_device_id(config: &Config) -> String {
        if let Some(id) = config.device_id.as_deref() {
            return id.to_string();
        }

        let path = Self::device_id_path();
        if let Ok(id) = std::fs::read_to_string(&path) {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        let id = hex::encode(rand::random::<[u8; 20]>());
        if std::fs::create_dir_all(Self::cache_dir()).is_ok() {
            let _ = std::fs::write(&path, id.as_bytes());
        }
        id
    }

    fn create_cache() -> Result<Cache, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Cache::new(Some(Self::cache_dir()), None, None, None)?)
    }

    fn create_session(cache: &Cache, device_id: &str) -> Session {
        let session_config = SessionConfig {
            device_id: device_id.to_string(),
            ..SessionConfig::default()
        };
        Session::new(session_config, Some(cache.clone()))
    }

    fn connect_config(device_name: &str) -> ConnectConfig {
        ConnectConfig {
            name: device_name.to_string(),
            device_type: DeviceType::Computer,
            is_group: false,
            initial_volume: 32767,
            disable_volume: false,
            volume_steps: 64,
        }
    }

    async fn fetch_track_info(
        session: &Session,
        track_uri: &SpotifyUri,
    ) -> Option<(String, String)> {
        match track_uri {
            SpotifyUri::Track { .. } => {
                let track = Track::get(session, track_uri).await.ok()?;
                let artist_names: Vec<_> = track
                    .artists
                    .iter()
                    .take(2)
                    .map(|a| a.name.clone())
                    .collect();
                let artist = if artist_names.is_empty() {
                    "Unknown artist".to_string()
                } else {
                    artist_names.join(", ")
                };
                Some((track.name, artist))
            }
            SpotifyUri::Episode { .. } => {
                let episode = Episode::get(session, track_uri).await.ok()?;
                Some((episode.name, "Podcast".to_string()))
            }
            _ => None,
        }
    }

    fn spawn_event_loop(
        rx: tokio::sync::mpsc::UnboundedReceiver<PlayerEvent>,
        session_for_meta: Session,
        bridge_for_events: Arc<AudioBridge>,
        presence_tx_events: mpsc::UnboundedSender<PresenceUpdate>,
        access_token: String,
        end_of_track_tx: Option<mpsc::UnboundedSender<()>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut rx = rx;
            let mut last_track: Option<SpotifyUri> = None;
            let mut last_title = String::new();
            let mut last_artist = String::new();
            let mut last_track_id = String::new();

            while let Some(event) = rx.recv().await {
                match event {
                    PlayerEvent::Playing { track_id, .. } => {
                        let is_new_track = last_track.as_ref() != Some(&track_id);
                        if is_new_track {
                            last_track_id = extract_track_id(&track_id);
                            if let Some((title, artist)) =
                                Self::fetch_track_info(&session_for_meta, &track_id).await
                            {
                                println!("Playing: {} - {}", title, artist);
                                last_title = title;
                                last_artist = artist;
                            } else {
                                println!("Playing: (unknown track)");
                                last_title.clear();
                                last_artist.clear();
                            }
                            last_track = Some(track_id);
                        }
                        let title = if last_title.is_empty() {
                            "Unknown track".to_string()
                        } else {
                            last_title.clone()
                        };
                        let artist = if last_artist.is_empty() {
                            "Unknown artist".to_string()
                        } else {
                            last_artist.clone()
                        };
                        let _ = presence_tx_events.send(PresenceUpdate::Playing {
                            title,
                            artist,
                            track_id: last_track_id.clone(),
                            access_token: access_token.clone(),
                        });
                    }
                    PlayerEvent::Paused { .. } => {
                        let _ = presence_tx_events.send(PresenceUpdate::Paused);
                        bridge_for_events.clear();
                        tracing::debug!("playback paused");
                    }
                    PlayerEvent::Stopped { .. } => {
                        let _ = presence_tx_events.send(PresenceUpdate::Idle);
                        bridge_for_events.clear();
                        tracing::debug!("playback stopped");
                    }
                    PlayerEvent::TrackChanged { .. } => {
                        last_track = None;
                    }
                    PlayerEvent::Loading { .. } => {
                        tracing::debug!("loading track");
                    }
                    PlayerEvent::EndOfTrack { .. } => {
                        // Don't clear the bridge on a natural track boundary — it
                        // would trim the tail of an auto-advancing track. A real
                        // stop is handled by PlayerEvent::Stopped, and a priority
                        // item's drain clears the bridge itself before playing.
                        let _ = presence_tx_events.send(PresenceUpdate::Idle);
                        if let Some(ref tx) = end_of_track_tx {
                            let _ = tx.send(());
                        }
                    }
                    PlayerEvent::Unavailable { .. } => {
                        let _ = presence_tx_events.send(PresenceUpdate::Idle);
                        bridge_for_events.clear();
                        println!("Track unavailable");
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
        presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
        access_token: String,
        end_of_track_tx: Option<mpsc::UnboundedSender<()>>,
        spirc_cmd_rx: &mut Option<mpsc::UnboundedReceiver<SpircCommand>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let cache = Self::create_cache()?;
        let device_id = Self::resolve_device_id(config);
        let device_name = config.device_name.clone();
        let mut reconnects: usize = 0;

        let credentials = Credentials::with_access_token(access_token.clone());

        loop {
            bridge.clear();
            let session = Self::create_session(&cache, &device_id);
            let connect_config = Self::connect_config(&device_name);
            let mixer: Arc<dyn Mixer> = Arc::new(
                SoftMixer::open(MixerConfig::default()).expect("Failed to create audio mixer"),
            );

            let player_config = PlayerConfig {
                bitrate: librespot_playback::config::Bitrate::Bitrate320,
                ..Default::default()
            };

            let bridge_clone = bridge.clone();
            let bridge_for_events = bridge.clone();
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
            // than detaching and pushing into the shared bridge as a ghost.
            let _event_guard = AbortOnDrop(Self::spawn_event_loop(
                rx,
                session.clone(),
                bridge_for_events,
                presence_tx.clone(),
                access_token.clone(),
                end_of_track_tx.clone(),
            ));

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

            if let Err(e) = spirc.activate() {
                tracing::warn!(error = ?e, "device activation failed");
            }

            tracing::info!("spotify connect active (oauth)");
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
                            Some(SpircCommand::Pause) => { let _ = spirc.pause(); }
                            Some(SpircCommand::Play)  => { let _ = spirc.play();  }
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

            if reconnects < MAX_CACHED_RECONNECTS {
                reconnects += 1;
                let delay = std::time::Duration::from_secs(2u64.saturating_mul(reconnects as u64));
                tracing::info!(attempt = reconnects, delay = ?delay, "oauth session dropped, fast reconnect");
                tokio::time::sleep(delay).await;
                continue;
            }

            tracing::info!("max reconnects reached, returning for token refresh");
            let _ = presence_tx.send(PresenceUpdate::Idle);
            return Ok(());
        }
    }
}
