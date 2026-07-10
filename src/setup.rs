use crate::config::Config;
use dialoguer::{Confirm, Input, Password, Select};
use serenity::all::{ChannelType, Http};
use std::path::Path;

const DISCORD_DEV_PORTAL: &str = "https://discord.com/developers/applications";
const BOT_PERMISSIONS: u64 = 7340032; // Connect + Speak + Mute Members
const MAX_TOKEN_ATTEMPTS: u32 = 3;

#[derive(Debug)]
pub enum SetupError {
    InvalidToken,
    NoGuilds,
    NoAudioChannels,
    Cancelled,
    Io(std::io::Error),
    Dialoguer(dialoguer::Error),
    Discord(Box<serenity::Error>),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::InvalidToken => write!(f, "invalid bot token after multiple attempts"),
            SetupError::NoGuilds => write!(f, "bot is not in any servers"),
            SetupError::NoAudioChannels => {
                write!(f, "no voice or stage channels found in the selected server")
            }
            SetupError::Cancelled => write!(f, "setup cancelled"),
            SetupError::Io(e) => write!(f, "io error: {e}"),
            SetupError::Dialoguer(e) => write!(f, "prompt error: {e}"),
            SetupError::Discord(e) => write!(f, "discord api error: {e}"),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<std::io::Error> for SetupError {
    fn from(e: std::io::Error) -> Self {
        SetupError::Io(e)
    }
}

impl From<dialoguer::Error> for SetupError {
    fn from(e: dialoguer::Error) -> Self {
        SetupError::Dialoguer(e)
    }
}

impl From<serenity::Error> for SetupError {
    fn from(e: serenity::Error) -> Self {
        SetupError::Discord(Box::new(e))
    }
}

pub async fn run_setup_wizard() -> Result<Config, SetupError> {
    println!();
    println!("=== Discord Spotify Player - Setup Wizard ===");
    println!();
    println!("This wizard will walk you through the initial configuration.");
    println!("You'll need a Discord bot token. Create one at:");
    println!("  {DISCORD_DEV_PORTAL}");
    println!();

    // Step 1: Bot token.
    let (token, http) = prompt_token().await?;

    // Step 2: Application info + invite URL.
    let app_info = http.get_current_application_info().await?;
    let app_id = app_info.id;
    let invite_url = format!(
        "https://discord.com/oauth2/authorize?client_id={}&scope=bot&permissions={}",
        app_id, BOT_PERMISSIONS
    );
    println!();
    println!("Invite your bot to a server using this link:");
    println!("  {invite_url}");
    println!();

    let confirmed = blocking(move || {
        Confirm::new()
            .with_prompt("Have you invited the bot to your server?")
            .default(true)
            .interact()
    })
    .await?;
    if !confirmed {
        println!("Invite the bot first, then re-run with --setup.");
        return Err(SetupError::Cancelled);
    }

    // Step 3: Select guild.
    let guilds = http.get_guilds(None, None).await?;
    if guilds.is_empty() {
        println!();
        println!("The bot isn't in any servers yet.");
        println!("Use the invite URL above, then re-run with --setup.");
        return Err(SetupError::NoGuilds);
    }

    let (guild_id, guild_name) = if guilds.len() == 1 {
        let guild = &guilds[0];
        println!();
        println!("Auto-selected server: {}", guild.name);
        (guild.id, guild.name.clone())
    } else {
        let names: Vec<String> = guilds.iter().map(|guild| guild.name.clone()).collect();
        let idx = blocking(move || {
            Select::new()
                .with_prompt("Which server?")
                .items(&names)
                .default(0)
                .interact()
        })
        .await?;
        (guilds[idx].id, guilds[idx].name.clone())
    };

    // Step 4: Select audio channel.
    let channels = http.get_channels(guild_id).await?;
    let audio_channels: Vec<_> = channels
        .iter()
        .filter(|channel| matches!(channel.kind, ChannelType::Voice | ChannelType::Stage))
        .collect();

    if audio_channels.is_empty() {
        println!();
        println!("No voice or stage channels found in \"{guild_name}\".");
        println!("Create a voice channel in Discord first, then re-run with --setup.");
        return Err(SetupError::NoAudioChannels);
    }

    let (channel_id, channel_name, channel_kind) = if audio_channels.len() == 1 {
        let channel = audio_channels[0];
        println!(
            "Auto-selected {} channel: {}",
            channel_kind_label(channel.kind),
            channel.name
        );
        (channel.id, channel.name.clone(), channel.kind)
    } else {
        let names: Vec<String> = audio_channels
            .iter()
            .map(|channel| format!("{} ({})", channel.name, channel_kind_label(channel.kind)))
            .collect();
        let idx = blocking(move || {
            Select::new()
                .with_prompt("Which audio channel?")
                .items(&names)
                .default(0)
                .interact()
        })
        .await?;
        (
            audio_channels[idx].id,
            audio_channels[idx].name.clone(),
            audio_channels[idx].kind,
        )
    };

    // Step 5: Device name.
    let device_name: String = blocking(|| {
        Input::<String>::new()
            .with_prompt("Spotify Connect device name")
            .default("Discord Player".to_string())
            .interact_text()
    })
    .await?;

    // Step 6: Summary + confirm.
    println!();
    println!("--- Configuration Summary ---");
    println!("  Server:         {guild_name}");
    println!(
        "  Audio channel:  {} ({})",
        channel_name,
        channel_kind_label(channel_kind)
    );
    println!("  Device name:    {device_name}");
    let masked_token = if token.len() >= 10 {
        format!("{}...{}", &token[..6], &token[token.len() - 4..])
    } else {
        "(invalid?)".to_string()
    };
    println!("  Token:          {masked_token}");
    println!();

    let confirmed = blocking(|| {
        Confirm::new()
            .with_prompt("Write this configuration to .env?")
            .default(true)
            .interact()
    })
    .await?;
    if !confirmed {
        return Err(SetupError::Cancelled);
    }

    // Step 7: Write .env.
    let guild_id_u64 = guild_id.get();
    let channel_id_u64 = channel_id.get();
    write_env_file(&token, guild_id_u64, channel_id_u64, &device_name)?;
    println!();
    println!(".env written successfully!");

    // dotenvy::dotenv() only fills vars that are currently unset, so a prior
    // failed from_env in the same process would shadow the freshly-written
    // values. Set them explicitly so the reload below sees the new config.
    std::env::set_var("DISCORD_TOKEN", &token);
    std::env::set_var("DISCORD_GUILD_ID", guild_id_u64.to_string());
    std::env::set_var("DISCORD_CHANNEL_ID", channel_id_u64.to_string());
    std::env::set_var("DEVICE_NAME", &device_name);

    // Step 8: Load config and return.
    let config =
        Config::from_env().map_err(|e| SetupError::Io(std::io::Error::other(e.to_string())))?;
    Ok(config)
}

async fn prompt_token() -> Result<(String, Http), SetupError> {
    for attempt in 1..=MAX_TOKEN_ATTEMPTS {
        let token: String = blocking(|| {
            Password::new()
                .with_prompt("Paste your Discord bot token")
                .allow_empty_password(true)
                .interact()
        })
        .await?;

        let token = token.trim().to_string();
        if token.is_empty() {
            println!("  Token cannot be empty.");
            continue;
        }

        println!("  Validating token...");
        let http = Http::new(&token);
        match http.get_current_user().await {
            Ok(user) => {
                println!("  Authenticated as: {} ({})", user.name, user.id);
                return Ok((token, http));
            }
            Err(_) => {
                if attempt < MAX_TOKEN_ATTEMPTS {
                    println!(
                        "  Invalid token. Please try again ({}/{MAX_TOKEN_ATTEMPTS}).",
                        attempt + 1
                    );
                } else {
                    println!("  Invalid token after {MAX_TOKEN_ATTEMPTS} attempts.");
                    return Err(SetupError::InvalidToken);
                }
            }
        }
    }

    Err(SetupError::InvalidToken)
}

fn write_env_file(
    token: &str,
    guild_id: u64,
    channel_id: u64,
    device_name: &str,
) -> Result<(), SetupError> {
    let env_path = Path::new(".env");
    let guild_id_str = guild_id.to_string();
    let channel_id_str = channel_id.to_string();
    let wizard_keys: [(&str, &str); 4] = [
        ("DISCORD_TOKEN", token),
        ("DISCORD_GUILD_ID", &guild_id_str),
        ("DISCORD_CHANNEL_ID", &channel_id_str),
        ("DEVICE_NAME", device_name),
    ];

    if env_path.exists() {
        // Update existing .env in-place, preserving comments and other keys.
        let contents = std::fs::read_to_string(env_path)?;
        let mut output_lines: Vec<String> = Vec::new();
        let mut written_keys: std::collections::HashSet<&str> = std::collections::HashSet::new();

        for line in contents.lines() {
            // Exact key match on the part left of '=', so a key that is a
            // prefix of another (e.g. DISCORD_TOKEN vs DISCORD_TOKEN_ALT)
            // can't be rewritten by mistake.
            let line_key = line.split('=').next().map(str::trim);
            let mut matched = false;
            for &(key, value) in &wizard_keys {
                if line_key == Some(key) {
                    output_lines.push(format!("{key}={value}"));
                    written_keys.insert(key);
                    matched = true;
                    break;
                }
            }
            if !matched {
                output_lines.push(line.to_string());
            }
        }

        // Append any wizard keys that weren't already present.
        for &(key, value) in &wizard_keys {
            if !written_keys.contains(key) {
                output_lines.push(format!("{key}={value}"));
            }
        }

        std::fs::write(env_path, output_lines.join("\n") + "\n")?;
    } else {
        // Write a fresh .env template.
        let contents = format!(
            "\
# Discord Bot Token (from Bot page)
# Get this from: {DISCORD_DEV_PORTAL}
DISCORD_TOKEN={token}

# Discord Server ID
DISCORD_GUILD_ID={guild_id}

# Discord Voice Channel ID
DISCORD_CHANNEL_ID={channel_id}

# Device name shown in Spotify Connect
DEVICE_NAME={device_name}

# Spotify app client id (required for /login). Redirect URI:
# http://127.0.0.1:8766/callback. PKCE flow — no client secret needed.
# SPOTIFY_CLIENT_ID=

# Encryption key for stored OAuth tokens (any long random string).
# Unset = tokens stored unencrypted (startup warning).
# TOKEN_ENC_KEY=

# Optional: text channel for now-playing embeds (defaults to the voice channel)
# TEXT_CHANNEL_ID=

# Optional: stable device id to avoid duplicate devices (auto-generated if omitted)
# DEVICE_ID=

# Audio buffer length in seconds (1-12, default: 8)
# AUDIO_BUFFER_SECONDS=8

# Prebuffer time before audio starts in seconds (0.0-8.0, default: 2.0)
# PREBUFFER_SECONDS=2.0

# EQ / audio tuning (adjust to taste)
# PREAMP_DB=0.0
# BASS_BOOST_DB=0.0
# TREBLE_BOOST_DB=0.0

# Logging level (default: warn)
#   Simple:  trace | debug | info | warn | error
#   Custom:  any valid RUST_LOG filter string
# RUST_LOG=warn
"
        );
        std::fs::write(env_path, contents)?;
    }

    Ok(())
}

fn channel_kind_label(kind: ChannelType) -> &'static str {
    match kind {
        ChannelType::Stage => "stage",
        ChannelType::Voice => "voice",
        _ => "other",
    }
}

/// Run a blocking closure on the Tokio blocking thread pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, dialoguer::Error> + Send + 'static,
) -> Result<T, SetupError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SetupError::Io(std::io::Error::other(e)))?
        .map_err(SetupError::Dialoguer)
}
