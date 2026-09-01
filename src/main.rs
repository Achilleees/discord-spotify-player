mod audio;
mod audio_bridge;
mod config;
mod discord;
mod history;
mod oauth;
mod player;
mod presence;
mod queue;
mod setup;
mod spotify;
mod users;
mod youtube;

use audio_bridge::AudioBridge;
use config::Config;
use discord::DiscordBot;
use youtube::{check_ytdlp_available, check_ffmpeg_available};
use oauth::SpotifyOAuth;
use presence::PresenceUpdate;
use users::UserStore;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

fn app_centric_filter(level: &str) -> String {
    format!(
        "warn,discord_spotify_player={level},audio_stream={level},player={level},\
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

    // Check yt-dlp and ffmpeg availability
    let ytdlp_ok = check_ytdlp_available();
    let ffmpeg_ok = check_ffmpeg_available();
    if !ytdlp_ok { tracing::warn!("yt-dlp not found in PATH — /play command disabled"); }
    if !ffmpeg_ok { tracing::warn!("ffmpeg not found in PATH — /play command disabled"); }
    let ytdlp_available = ytdlp_ok && ffmpeg_ok;
    if ytdlp_available {
        tracing::info!("yt-dlp and ffmpeg available — YouTube/file playback enabled");
        // Ensure the YouTube scratch dir exists, and clear out partials left
        // by a previous crash or kill mid-download.
        let _ = std::fs::create_dir_all(youtube::tmp_dir());
        youtube::sweep_tmp_dir();
    }

    // OAuth (Authorization Code + PKCE, desktop client id) is the only
    // session path.
    let oauth: Arc<SpotifyOAuth> = Arc::new(SpotifyOAuth::new());
    tracing::info!("spotify oauth enabled (device authorization)");

    let db_path = std::env::var("SPOTIBOT_DB").unwrap_or_else(|_| "spotibot.db".to_string());
    let user_store = Arc::new(
        UserStore::open(&db_path, config.token_enc_key.as_deref())
            .map_err(|e| io::Error::other(format!("failed to open credential store: {e}")))?,
    );

    // History is a nice-to-have: if the table can't be opened the bot still
    // plays, it just stops keeping a record.
    let history = match history::HistoryStore::open(&db_path) {
        Ok(h) => Some(Arc::new(h)),
        Err(e) => {
            tracing::warn!(error = %e, "play history disabled — could not open the table");
            None
        }
    };

    let bridge = AudioBridge::new(config.audio_buffer_seconds);
    tracing::debug!("audio bridge initialized");

    let (presence_tx, presence_rx) = mpsc::unbounded_channel::<PresenceUpdate>();

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

    let config = Arc::new(config);

    let discord_bot = DiscordBot::new(
        config.clone(),
        bridge.clone(),
        presence_rx,
        presence_tx.clone(),
        user_store.clone(),
        history.clone(),
        oauth.clone(),
        ytdlp_available,
    )
    .await?;

    let mut ready_rx = discord_bot.start_background().await?;

    tracing::info!("waiting for discord connection");
    match ready_rx.recv().await {
        Some(Ok(())) => {}
        Some(Err(error)) => return Err(io::Error::other(error).into()),
        None => return Err(io::Error::other("discord startup channel closed unexpectedly").into()),
    }
    println!("Discord connected.");
    tracing::info!("discord ready");

    let _ = presence_tx.send(PresenceUpdate::Idle);

    // The bot owns everything from here: auto-start of a stored session runs
    // inside the ready() handler, sessions start via /login. Park forever.
    println!("Ready. Use /login in Discord to start a Spotify session.");
    std::future::pending::<()>().await;

    Ok(())
}
