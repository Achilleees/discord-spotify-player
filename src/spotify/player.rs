use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::presence::PresenceUpdate;
use crate::spotify::sink::{DiscordSink, DspConfig};
use futures_util::StreamExt;
use librespot::connect::spirc::Spirc;
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::{ConnectConfig, SessionConfig};
use librespot_core::session::Session;
use librespot_core::spotify_id::{SpotifyAudioType, SpotifyId};
use librespot_discovery::{DeviceType, Discovery};
use librespot_metadata::{Artist, Episode, Metadata, Track};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

const CACHE_DIR: &str = ".spotify_cache";
const DEVICE_ID_FILE: &str = "device_id";
const CONNECT_RETRIES: usize = 3;
const CONNECT_RETRY_DELAY_MS: u64 = 500;
const MAX_CACHED_RECONNECTS: usize = 3;
const RECONNECT_BASE_DELAY_MS: u64 = 750;
const MIN_STABLE_SESSION_SECS: u64 = 30;

enum CredentialOrigin {
    Discovery,
    Cache,
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

    fn start_discovery(
        device_id: &str,
        device_name: &str,
    ) -> Result<Discovery, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Discovery::builder(device_id.to_string())
            .name(device_name.to_string())
            .device_type(DeviceType::Computer)
            .launch()?)
    }

    async fn connect_session(
        cache: &Cache,
        device_id: &str,
        credentials: Credentials,
    ) -> Option<Session> {
        let mut session_config = SessionConfig::default();
        session_config.device_id = device_id.to_string();

        for attempt in 1..=CONNECT_RETRIES {
            match Session::connect(
                session_config.clone(),
                credentials.clone(),
                Some(cache.clone()),
                false,
            )
            .await
            {
                Ok((session, _reusable_credentials)) => return Some(session),
                Err(e) => {
                    tracing::warn!(
                        "Connection attempt {}/{} failed: {:?}",
                        attempt,
                        CONNECT_RETRIES,
                        e
                    );
                    if attempt < CONNECT_RETRIES {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            CONNECT_RETRY_DELAY_MS,
                        ))
                        .await;
                    }
                }
            }
        }

        None
    }

    fn connect_config(device_name: &str) -> ConnectConfig {
        ConnectConfig {
            name: device_name.to_string(),
            device_type: DeviceType::Computer,
            initial_volume: Some(65535 / 2),
            has_volume_ctrl: true,
            autoplay: false,
        }
    }

    async fn fetch_track_info(
        session: &Session,
        track_id: SpotifyId,
    ) -> Option<(String, String)> {
        match track_id.audio_type {
            SpotifyAudioType::Track => {
                let track = Track::get(session, track_id).await.ok()?;
                let mut artists = Vec::new();
                for artist_id in track.artists.iter().take(2) {
                    if let Ok(artist) = Artist::get(session, *artist_id).await {
                        artists.push(artist.name);
                    }
                }
                let artist = if artists.is_empty() {
                    "Unknown artist".to_string()
                } else {
                    artists.join(", ")
                };
                Some((track.name, artist))
            }
            SpotifyAudioType::Podcast => {
                let episode = Episode::get(session, track_id).await.ok()?;
                Some((episode.name, "Podcast".to_string()))
            }
            SpotifyAudioType::NonPlayable => None,
        }
    }

    /// Run Spotify Connect using discovery mode
    /// This announces the device on the local network, and Spotify apps can connect to it
    pub async fn run_discovery(
        config: &Config,
        bridge: Arc<AudioBridge>,
        presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!("Starting Spotify Connect discovery...");
        tracing::info!("Device name: '{}'", config.device_name);

        let cache = Self::create_cache()?;
        let device_id = Self::resolve_device_id(config);
        let device_name = config.device_name.clone();
        let mut discovery = Self::start_discovery(&device_id, &device_name)?;

        println!("Spotify device '{}' is ready. Select it in Spotify.", device_name);
        tracing::info!("Spotify Connect device is now discoverable!");
        tracing::info!(
            "Open Spotify on your phone/computer and look for '{}' in the device list.",
            device_name
        );

        let mut pending_cached = cache.credentials();
        let mut cached_reconnects: usize = 0;

        loop {
            let (credentials, origin) = if let Some(creds) = pending_cached.take() {
                if cached_reconnects > 0 {
                    let backoff_ms = RECONNECT_BASE_DELAY_MS
                        .saturating_mul(1u64 << (cached_reconnects.saturating_sub(1) as u32));
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                }
                (creds, CredentialOrigin::Cache)
            } else {
                match discovery.next().await {
                    Some(creds) => {
                        cache.save_credentials(&creds);
                        cached_reconnects = 0;
                        (creds, CredentialOrigin::Discovery)
                    }
                    None => break,
                }
            };

            match origin {
                CredentialOrigin::Discovery => {
                    println!("Spotify paired. Connecting...");
                    tracing::info!("Received credentials from Spotify client!");
                }
                CredentialOrigin::Cache => {
                    println!("Spotify reconnecting...");
                    tracing::info!("Attempting Spotify reconnect using cached credentials.");
                }
            }

            let session = match Self::connect_session(&cache, &device_id, credentials).await {
                Some(session) => session,
                None => {
                    match origin {
                        CredentialOrigin::Cache => {
                            tracing::warn!("Cached credentials failed. Waiting for new pairing...");
                        }
                        CredentialOrigin::Discovery => {
                            tracing::error!(
                                "Failed to connect to Spotify after {} attempts",
                                CONNECT_RETRIES
                            );
                        }
                    }
                    continue;
                }
            };

            println!("Spotify connected.");
            tracing::info!("Connected to Spotify!");

            let session_start = std::time::Instant::now();
            let connect_config = Self::connect_config(&device_name);
            let mixer: Box<dyn Mixer> = Box::new(SoftMixer::open(MixerConfig::default()));

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
            let (player, mut rx) = Player::new(
                player_config,
                session.clone(),
                mixer.get_soft_volume(),
                {
                    let dsp_config = dsp_config;
                    move || Box::new(DiscordSink::new(bridge_clone.clone(), dsp_config))
                },
            );
            let presence_tx_events = presence_tx.clone();
            let session_for_meta = session.clone();
            tokio::spawn(async move {
                let mut last_track: Option<SpotifyId> = None;
                let mut last_title = String::new();
                let mut last_artist = String::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        PlayerEvent::Playing { track_id, .. } => {
                            if last_track.as_ref() != Some(&track_id) {
                                if let Some((title, artist)) =
                                    Self::fetch_track_info(&session_for_meta, track_id).await
                                {
                                    last_track = Some(track_id);
                                    last_title = title;
                                    last_artist = artist;
                                } else {
                                    last_track = Some(track_id);
                                    last_title.clear();
                                    last_artist.clear();
                                }
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
                            let _ = presence_tx_events.send(PresenceUpdate::Playing { title, artist });
                            println!("Playback started.");
                        }
                        PlayerEvent::Paused { .. } => {
                            let _ = presence_tx_events.send(PresenceUpdate::Paused);
                            bridge_for_events.clear();
                            println!("Playback paused.");
                        }
                        PlayerEvent::Stopped { .. } => {
                            let _ = presence_tx_events.send(PresenceUpdate::Idle);
                            bridge_for_events.clear();
                            println!("Playback stopped.");
                        }
                        PlayerEvent::Changed { .. } => {
                            last_track = None;
                            println!("Track changed.");
                        }
                        PlayerEvent::Loading { .. } => println!("Loading track..."),
                        PlayerEvent::EndOfTrack { .. } => {
                            let _ = presence_tx_events.send(PresenceUpdate::Idle);
                            bridge_for_events.clear();
                            println!("Track ended.");
                        }
                        PlayerEvent::Unavailable { .. } => {
                            let _ = presence_tx_events.send(PresenceUpdate::Idle);
                            bridge_for_events.clear();
                            println!("Track unavailable.");
                        }
                        _ => {}
                    }
                }
            });

            let (spirc, spirc_task) = Spirc::new(connect_config, session.clone(), player, mixer);

            tracing::info!("Spotify Connect is now active! Play music and it will stream to Discord.");

            spirc_task.await;
            drop(spirc);

            let session_duration = session_start.elapsed();
            if session_duration >= std::time::Duration::from_secs(MIN_STABLE_SESSION_SECS) {
                cached_reconnects = 0;
            }

            let _ = presence_tx.send(PresenceUpdate::Idle);

            if let Some(creds) = cache.credentials() {
                if cached_reconnects < MAX_CACHED_RECONNECTS {
                    cached_reconnects += 1;
                    pending_cached = Some(creds);
                    tracing::info!(
                        "Spotify session ended. Scheduling reconnect attempt {}/{}.",
                        cached_reconnects,
                        MAX_CACHED_RECONNECTS
                    );
                } else {
                    tracing::info!("Spotify session ended. Waiting for new connection...");
                    pending_cached = None;
                    cached_reconnects = 0;
                }
            } else {
                tracing::info!("Spotify session ended. Waiting for new connection...");
            }
        }

        Ok(())
    }
}
