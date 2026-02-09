mod audio_bridge;
mod config;
mod discord;
mod presence;
mod setup;
mod spotify;

use audio_bridge::AudioBridge;
use config::Config;
use discord::DiscordBot;
use presence::PresenceUpdate;
use spotify::SpotifyPlayer;
use tokio::sync::mpsc;

/// Build a filter string that sets the app crate to `level` and keeps noisy
/// dependencies at `warn`. The base level is `warn` so only our crate gets
/// the verbose output.
fn app_centric_filter(level: &str) -> String {
    format!(
        "warn,discord_spotify_player={level},audio_stream={level},\
         serenity=warn,songbird=warn,librespot=warn"
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging.
    //   - No RUST_LOG          -> warn for all (clean output, only println messages visible)
    //   - RUST_LOG=<level>     -> app-centric preset (app gets that level, deps stay warn)
    //   - Anything else        -> pass through as a custom EnvFilter string
    let env_filter = match std::env::var("RUST_LOG") {
        Err(_) => tracing_subscriber::EnvFilter::new(app_centric_filter("warn")),
        Ok(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            match trimmed.as_str() {
                "trace" => tracing_subscriber::EnvFilter::new(app_centric_filter("trace")),
                "debug" => tracing_subscriber::EnvFilter::new(app_centric_filter("debug")),
                "info" => tracing_subscriber::EnvFilter::new(app_centric_filter("info")),
                "warn" => tracing_subscriber::EnvFilter::new(app_centric_filter("warn")),
                "error" => tracing_subscriber::EnvFilter::new(app_centric_filter("error")),
                _ => tracing_subscriber::EnvFilter::new(value),
            }
        }
    };
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    tracing::info!("starting discord spotify player");

    // Load configuration. Run wizard on --setup or when .env is missing/invalid.
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

    // Create shared audio bridge.
    let bridge = AudioBridge::new(config.audio_buffer_seconds);
    tracing::debug!("audio bridge initialized");

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
                buf_len = bridge_stats.len(),
                pushed  = stats.total_pushed,
                pulled  = stats.total_pulled,
                dropped = stats.total_dropped,
                last_push_ms        = stats.last_push_ms,
                last_pull_ms        = stats.last_pull_ms,
                last_nonzero_pull_ms = stats.last_nonzero_pull_ms,
                "bridge stats"
            );
        }
    });

    // Start Discord bot (connects to voice channel).
    let discord_bot = DiscordBot::new(&config, bridge.clone(), presence_rx).await?;
    let mut ready_rx = discord_bot.start_background().await?;

    // Wait for Discord to be ready.
    tracing::info!("waiting for discord connection");
    ready_rx.recv().await;
    println!("Discord connected. Waiting for Spotify Connect pairing...");
    tracing::info!("discord ready");

    // Run Spotify Connect discovery (this will block).
    let _ = presence_tx.send(PresenceUpdate::Idle);
    SpotifyPlayer::run_discovery(&config, bridge, presence_tx).await?;

    Ok(())
}
