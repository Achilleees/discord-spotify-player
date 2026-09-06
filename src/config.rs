use crate::runtime::{Profile, Settings};

#[derive(Clone)]
pub struct Config {
    pub profile: Profile,
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
    /// Base64 or hex 32-byte key for encrypting stored OAuth tokens at rest.
    /// When absent, tokens are stored unencrypted (with a startup warning).
    pub token_enc_key: Option<String>,
}

/// Parse a numeric env var, warning (rather than silently defaulting) when a
/// value is present but unparseable — so a typo is distinguishable from unset.
fn env_num<T: std::str::FromStr>(settings: &Settings, key: &str) -> Option<T> {
    parse_num(key, settings.get(key))
}

/// The pure half of [`env_num`]: None for unset/blank, Some for a valid parse,
/// and a warn + None for a present-but-unparseable value.
fn parse_num<T: std::str::FromStr>(key: &str, raw: Option<&str>) -> Option<T> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(key, value = %trimmed, "invalid numeric config value; using default");
            None
        }
    }
}

/// Resolve the embeds/controls text channel: a valid TEXT_CHANNEL_ID wins,
/// unset or invalid falls back to the voice channel's built-in text chat.
fn resolve_text_channel_id(raw: Option<&str>, voice_channel_id: u64) -> u64 {
    raw.and_then(|v| parse_id("TEXT_CHANNEL_ID", v).ok())
        .unwrap_or(voice_channel_id)
}

/// Parse a Discord snowflake id, rejecting zero (serenity's Id::new panics on 0).
fn parse_id(key: &'static str, raw: &str) -> Result<u64, ConfigError> {
    let id: u64 = raw.trim().parse().map_err(|_| ConfigError::Invalid(key))?;
    if id == 0 {
        return Err(ConfigError::Invalid(key));
    }
    Ok(id)
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let settings = Settings::load(Profile::Spotibot, None)
            .map_err(|_| ConfigError::Invalid("env file"))?;
        Self::from_settings(&settings)
    }

    pub fn from_settings(settings: &Settings) -> Result<Self, ConfigError> {
        let preamp_db = env_num::<f32>(settings, "PREAMP_DB")
            .unwrap_or(0.0)
            .clamp(-12.0, 12.0);
        let bass_boost_db = env_num::<f32>(settings, "BASS_BOOST_DB")
            .unwrap_or(0.0)
            .clamp(0.0, 12.0);
        let treble_boost_db = env_num::<f32>(settings, "TREBLE_BOOST_DB")
            .unwrap_or(0.0)
            .clamp(-6.0, 6.0);
        let audio_buffer_seconds = env_num::<usize>(settings, "AUDIO_BUFFER_SECONDS")
            .unwrap_or(8)
            .clamp(1, 12);
        let prebuffer_seconds = env_num::<f32>(settings, "PREBUFFER_SECONDS")
            .unwrap_or(2.0)
            .clamp(0.0, 8.0);

        let device_id = settings
            .get("DEVICE_ID")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let token_enc_key = settings
            .get("TOKEN_ENC_KEY")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let discord_channel_id = parse_id(
            "DISCORD_CHANNEL_ID",
            settings
                .get("DISCORD_CHANNEL_ID")
                .ok_or(ConfigError::Missing("DISCORD_CHANNEL_ID"))?,
        )?;

        let discord_guild_id = parse_id(
            "DISCORD_GUILD_ID",
            settings
                .get("DISCORD_GUILD_ID")
                .ok_or(ConfigError::Missing("DISCORD_GUILD_ID"))?,
        )?;

        let discord_text_channel_id =
            resolve_text_channel_id(settings.get("TEXT_CHANNEL_ID"), discord_channel_id);

        let discord_token = settings
            .get("DISCORD_TOKEN")
            .filter(|v| !v.trim().is_empty())
            .ok_or(ConfigError::Missing("DISCORD_TOKEN"))?
            .trim()
            .to_string();
        Ok(Config {
            profile: settings.profile,
            discord_token,
            discord_guild_id,
            discord_channel_id,
            discord_text_channel_id,
            device_name: settings
                .get("DEVICE_NAME")
                .unwrap_or(if settings.profile == Profile::Nob {
                    "nob"
                } else {
                    "Discord Player"
                })
                .to_string(),
            device_id,
            audio_buffer_seconds,
            prebuffer_seconds,
            preamp_db,
            bass_boost_db,
            treble_boost_db,
            token_enc_key,
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

#[cfg(test)]
mod tests {
    use super::{parse_id, parse_num, resolve_text_channel_id};

    #[test]
    fn accepts_valid_snowflake() {
        assert_eq!(
            parse_id("X", "123456789012345678").unwrap(),
            123456789012345678
        );
    }

    #[test]
    fn rejects_zero() {
        assert!(parse_id("X", "0").is_err());
    }

    #[test]
    fn rejects_non_numeric() {
        assert!(parse_id("X", "not-a-number").is_err());
        assert!(parse_id("X", "").is_err());
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_id("X", "  123  ").unwrap(), 123);
    }

    #[test]
    fn parse_num_distinguishes_unset_blank_and_invalid() {
        // Unset and blank are silent defaults …
        assert_eq!(parse_num::<f32>("X", None), None);
        assert_eq!(parse_num::<f32>("X", Some("")), None);
        assert_eq!(parse_num::<f32>("X", Some("   ")), None);
        // … a typo also yields None (with a warn in the real path) …
        assert_eq!(parse_num::<f32>("X", Some("3.o")), None);
        assert_eq!(parse_num::<usize>("X", Some("-1")), None);
        // … and a valid value parses, whitespace tolerated.
        assert_eq!(parse_num::<f32>("X", Some(" 3.5 ")), Some(3.5));
        assert_eq!(parse_num::<usize>("X", Some("8")), Some(8));
    }

    #[test]
    fn text_channel_falls_back_to_voice_channel() {
        // Valid override wins.
        assert_eq!(resolve_text_channel_id(Some("42"), 7), 42);
        // Unset, invalid, and zero all fall back to the voice channel chat.
        assert_eq!(resolve_text_channel_id(None, 7), 7);
        assert_eq!(resolve_text_channel_id(Some("not-an-id"), 7), 7);
        assert_eq!(resolve_text_channel_id(Some("0"), 7), 7);
    }
}
