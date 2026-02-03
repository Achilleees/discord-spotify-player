mod audio_bridge;
mod config;
mod discord;
mod presence;
mod spotify;

use audio_bridge::AudioBridge;
use config::Config;
use discord::DiscordBot;
use presence::PresenceUpdate;
use spotify::SpotifyPlayer;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging with a quiet-by-default filter.
    let env_filter = match std::env::var("RUST_LOG") {
        Ok(level) if level.trim().eq_ignore_ascii_case("trace") => {
            tracing_subscriber::EnvFilter::new(
                "info,discord_spotify_player=trace,serenity=warn,songbird=warn,librespot=warn",
            )
        }
        Ok(level) if level.trim().eq_ignore_ascii_case("debug") => {
            tracing_subscriber::EnvFilter::new(
                "info,discord_spotify_player=debug,serenity=warn,songbird=warn,librespot=warn",
            )
        }
        _ => tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "info,discord_spotify_player=info,serenity=warn,songbird=warn,librespot=warn",
            )
        }),
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();

    println!("Hello! Discord Spotify Player starting...");
    tracing::info!("Starting Discord Spotify Player...");

    // Load configuration
    let config = Config::from_env()?;
    tracing::info!("Configuration loaded");

    // Create shared audio bridge
    let bridge = AudioBridge::new(config.audio_buffer_seconds);
    tracing::info!("Audio bridge initialized");

    let (presence_tx, presence_rx) = mpsc::unbounded_channel();

    // Periodic bridge stats for diagnostics (low-frequency, safe in production).
    let bridge_stats = bridge.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let stats = bridge_stats.stats_snapshot();
            tracing::debug!(
                target: "audio_stream",
                "Bridge stats: buf_len={}, pushed={}, pulled={}, dropped={}, last_push_ms={}, last_pull_ms={}, last_nonzero_pull_ms={}",
                bridge_stats.len(),
                stats.total_pushed,
                stats.total_pulled,
                stats.total_dropped,
                stats.last_push_ms,
                stats.last_pull_ms,
                stats.last_nonzero_pull_ms
            );
        }
    });

    // Start Discord bot (connects to voice channel)
    let discord_bot = DiscordBot::new(&config, bridge.clone(), presence_rx).await?;
    let mut ready_rx = discord_bot.start_background().await?;

    // Wait for Discord to be ready
    tracing::info!("Waiting for Discord connection...");
    ready_rx.recv().await;
    println!("Discord connected. Waiting for Spotify Connect pairing...");
    tracing::info!("Discord ready!");

    // Run Spotify Connect discovery (this will block)
    let _ = presence_tx.send(PresenceUpdate::Idle);
    SpotifyPlayer::run_discovery(&config, bridge, presence_tx).await?;

    Ok(())
}
