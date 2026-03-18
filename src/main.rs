mod audio;
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
        oauth.clone(),
    )
    .await?;

    let (mut ready_rx, active_session) = discord_bot.start_background().await?;

    tracing::info!("waiting for discord connection");
    match ready_rx.recv().await {
        Some(Ok(())) => {}
        Some(Err(error)) => return Err(io::Error::other(error).into()),
        None => return Err(io::Error::other("discord startup channel closed unexpectedly").into()),
    }
    println!("Discord connected.");
    tracing::info!("discord ready");

    let _ = presence_tx.send(PresenceUpdate::Idle);

    // Auto-start: if any user has stored credentials marked active, start their session.
    let active_users: Vec<_> = user_store.list().into_iter().filter(|u| u.active).collect();
    if !active_users.is_empty() {
        if let Some(oauth_client) = &oauth {
            if let Some(user) = active_users.into_iter().next() {
                tracing::info!(
                    spotify = %user.spotify_username,
                    "auto-starting OAuth session for stored active user"
                );
                println!(
                    "Auto-starting Spotify session for {}...",
                    user.spotify_username
                );

                let token = match oauth_client.refresh_access_token(&user.refresh_token).await {
                    Ok(t) => {
                        // Persist the refreshed token
                        let mut updated = user.clone();
                        updated.access_token = t.access_token.clone();
                        if let Some(rt) = t.refresh_token.clone() {
                            updated.refresh_token = rt;
                        }
                        let _ = user_store.save(&updated);
                        t.access_token
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            "failed to refresh token for auto-start; falling back to stored token"
                        );
                        user.access_token.clone()
                    }
                };

                let config_for_task = config.clone();
                let bridge_for_task = bridge.clone();
                let presence_tx_for_task = presence_tx.clone();
                let discord_user_id = user
                    .discord_user_id
                    .parse::<u64>()
                    .unwrap_or(0);
                let spotify_name = user.spotify_username.clone();
                let access_token_for_session = token.clone();

                let handle = tokio::spawn(async move {
                    match SpotifyPlayer::run_with_token(
                        &config_for_task,
                        bridge_for_task,
                        presence_tx_for_task,
                        token,
                    )
                    .await
                    {
                        Ok(()) => tracing::info!("auto-start session ended cleanly"),
                        Err(e) => tracing::warn!(error = ?e, "auto-start session ended with error"),
                    }
                });

                {
                    let mut lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
                    *lock = Some(discord::ActiveSession {
                        discord_user_id,
                        spotify_name,
                        access_token: access_token_for_session,
                        handle,
                    });
                } // lock dropped here

                // Park main task — the bot runs indefinitely
                std::future::pending::<()>().await;
            }
        } else {
            tracing::warn!("stored active users found but OAuth not configured — falling back to discovery");
            SpotifyPlayer::run_discovery(&config, bridge, presence_tx).await?;
        }
    } else {
        println!("No stored OAuth sessions. Waiting for Spotify Connect pairing (discovery mode)...");
        SpotifyPlayer::run_discovery(&config, bridge, presence_tx).await?;
    }

    Ok(())
}
