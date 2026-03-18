mod audio_bridge;
mod config;
mod discord;
mod oauth;
mod presence;
mod setup;
mod spotify;
mod users;

use audio_bridge::AudioBridge;
use config::Config;
use discord::DiscordBot;
use oauth::SpotifyOAuth;
use presence::PresenceUpdate;
use spotify::SpotifyPlayer;
use users::UserStore;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

fn app_centric_filter(level: &str) -> String {
    format!(
        "warn,discord_spotify_player={level},audio_stream={level},\
         serenity=warn,songbird=warn,librespot=warn,\
         librespot_connect::state::context=error,\
         symphonia_bundle_mp3=error"
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let env_filter = match std::env::var("RUST_LOG") {
        Err(_) => tracing_subscriber::EnvFilter::new(app_centric_filter("warn")),
        Ok(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            match trimmed.as_str() {
                "trace" => tracing_subscriber::EnvFilter::new(app_centric_filter("trace")),
                "debug" => tracing_subscriber::EnvFilter::new(app_centric_filter("debug")),
                "info"  => tracing_subscriber::EnvFilter::new(app_centric_filter("info")),
                "warn"  => tracing_subscriber::EnvFilter::new(app_centric_filter("warn")),
                "error" => tracing_subscriber::EnvFilter::new(app_centric_filter("error")),
                _       => tracing_subscriber::EnvFilter::new(value),
            }
        }
    };
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    tracing::info!("starting discord spotify player");

    let config = if std::env::args().any(|arg| arg == "--setup") {
        setup::run_setup_wizard().await?
    } else {
        match Config::from_env() {
            Ok(config) => config,
            Err(err) => {
                println!("Configuration missing or invalid: {err}");
                println!("Launching setup wizard...");
                setup::run_setup_wizard().await?
            }
        }
    };

    println!();
    println!("Discord Spotify Player v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("configuration loaded");

    // Build OAuth handler if credentials are configured
    let oauth: Option<Arc<SpotifyOAuth>> = match (
        config.spotify_client_id.clone(),
        config.spotify_client_secret.clone(),
    ) {
        (Some(id), Some(secret)) => {
            tracing::info!("Spotify OAuth enabled (client_id: {})", &id[..8.min(id.len())]);
            Some(Arc::new(SpotifyOAuth::new(id, secret)))
        }
        _ => {
            tracing::info!("Spotify OAuth not configured (SPOTIFY_CLIENT_ID/SECRET not set)");
            None
        }
    };

    let user_store = Arc::new(UserStore::new());

    let bridge = AudioBridge::new(config.audio_buffer_seconds);
    tracing::debug!("audio bridge initialized");

    let (presence_tx, presence_rx) = mpsc::unbounded_channel();

    let bridge_stats = bridge.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let stats = bridge_stats.stats_snapshot();
            tracing::debug!(
                target: "audio_stream",
                buf_len = bridge_stats.len(),
                pushed  = stats.total_pushed,
                pulled  = stats.total_pulled,
                dropped = stats.total_dropped,
                last_push_ms         = stats.last_push_ms,
                last_pull_ms         = stats.last_pull_ms,
                last_nonzero_pull_ms = stats.last_nonzero_pull_ms,
                "bridge stats"
            );
        }
    });

    let discord_bot = DiscordBot::new(&config, bridge.clone(), presence_rx, user_store, oauth).await?;
    let mut ready_rx = discord_bot.start_background().await?;

    tracing::info!("waiting for discord connection");
    match ready_rx.recv().await {
        Some(Ok(())) => {}
        Some(Err(error)) => return Err(io::Error::other(error).into()),
        None => return Err(io::Error::other("discord startup channel closed unexpectedly").into()),
    }
    println!("Discord connected. Waiting for Spotify Connect pairing...");
    tracing::info!("discord ready");

    let _ = presence_tx.send(PresenceUpdate::Idle);
    SpotifyPlayer::run_discovery(&config, bridge, presence_tx).await?;

    Ok(())
}
