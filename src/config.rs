use std::env;

#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_guild_id: u64,
    pub discord_channel_id: u64,
    pub discord_text_channel_id: u64,
    pub device_name: String,
    pub device_id: Option<String>,
    pub audio_buffer_seconds: usize,
    pub prebuffer_seconds: f32,
    pub preamp_db: f32,
    pub bass_boost_db: f32,
    pub treble_boost_db: f32,
    pub spotify_client_id: Option<String>,
    pub spotify_client_secret: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        dotenvy::dotenv().ok();

        let preamp_db = env::var("PREAMP_DB")
            .ok()
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| value.clamp(-12.0, 12.0))
            .unwrap_or(0.0);

        let bass_boost_db = env::var("BASS_BOOST_DB")
            .ok()
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| value.clamp(0.0, 12.0))
            .unwrap_or(0.0);

        let treble_boost_db = env::var("TREBLE_BOOST_DB")
            .ok()
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| value.clamp(-6.0, 6.0))
            .unwrap_or(0.0);

        let audio_buffer_seconds = env::var("AUDIO_BUFFER_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .map(|value| value.clamp(1, 12))
            .unwrap_or(8);

        let prebuffer_seconds = env::var("PREBUFFER_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| value.clamp(0.0, 8.0))
            .unwrap_or(2.0);

        let device_id = env::var("DEVICE_ID")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let spotify_client_id = env::var("SPOTIFY_CLIENT_ID")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let spotify_client_secret = env::var("SPOTIFY_CLIENT_SECRET")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let discord_channel_id: u64 = env::var("DISCORD_CHANNEL_ID")
            .map_err(|_| ConfigError::Missing("DISCORD_CHANNEL_ID"))?
            .parse()
            .map_err(|_| ConfigError::Invalid("DISCORD_CHANNEL_ID"))?;

        // Text channel for embeds/controls; falls back to the voice channel's
        // built-in text chat when unset.
        let discord_text_channel_id = env::var("TEXT_CHANNEL_ID")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(discord_channel_id);

        Ok(Config {
            discord_token: env::var("DISCORD_TOKEN")
                .map_err(|_| ConfigError::Missing("DISCORD_TOKEN"))?,
            discord_guild_id: env::var("DISCORD_GUILD_ID")
                .map_err(|_| ConfigError::Missing("DISCORD_GUILD_ID"))?
                .parse()
                .map_err(|_| ConfigError::Invalid("DISCORD_GUILD_ID"))?,
            discord_channel_id,
            discord_text_channel_id,
            device_name: env::var("DEVICE_NAME").unwrap_or_else(|_| "Discord Player".to_string()),
            device_id,
            audio_buffer_seconds,
            prebuffer_seconds,
            preamp_db,
            bass_boost_db,
            treble_boost_db,
            spotify_client_id,
            spotify_client_secret,
        })
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(&'static str),
    Invalid(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Missing(var) => write!(f, "Missing environment variable: {}", var),
            ConfigError::Invalid(var) => {
                write!(f, "Invalid value for environment variable: {}", var)
            }
        }
    }
}

impl std::error::Error for ConfigError {}
