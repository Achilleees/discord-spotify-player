# Discord Spotify Player

A personal Discord bot that makes your Spotify session available in a voice channel. It shows up in Spotify as a normal Spotify Connect device and plays whatever you pick in the Discord call so everyone can hear it.

## Stack (high-level)
- Discord bot + voice: `serenity` and `songbird`.
- Spotify Connect device + playback: `librespot`.
- Runtime + logging: `tokio` and `tracing`.

## What it does
- Creates a Spotify Connect device with a friendly name.
- Streams Spotify playback into a specific Discord voice channel.
- Lets you control playback from any Spotify client (desktop, mobile, web).
- Keeps a local Spotify Connect session so pairing is quick after the first time.
- Uses a stable device ID to avoid duplicate devices in the Spotify list.

## Current scope and limitations
- Runs locally on your machine (there is no hosted service).
- Connects one Discord bot to one server and one voice channel at a time.
- No UI yet; configuration is via the CLI setup wizard or `.env`.

## Roadmap

Planned features (in progress):

| Feature | Branch | Status |
|---------|--------|--------|
| **Now Playing Channel** | `feat/now-playing-channel` | Planned |
| **Setup Wizard** | `feat/setup-wizard` | Merged |
| **YouTube Support** | `feat/youtube-support` | Planned |

### Now Playing Channel
A dedicated text channel that displays the current track with a rich embed (album art, track info, Spotify link). Includes sticky playback controls (play/pause/skip buttons) so you can control Spotify without switching apps.

### Setup Wizard
Interactive first-run CLI that guides new users through configuration. Paste your Discord token, and it fetches your servers and channels automatically - no more manually copying IDs.

### YouTube Support
Play YouTube links or search YouTube directly from Discord. Audio routes through the same voice channel alongside Spotify.

## What it does not do (yet)
- No library browsing or search inside Discord (YouTube search planned).
- No recording, downloads, or file storage of audio.
- No multi-channel or multi-server routing.

## Requirements and expectations
- Spotify Premium account (required for Spotify Connect playback).
- A Discord bot account with permission to join and speak in a voice channel.
- The machine running the bot should be on the same network as the Spotify client for discovery to be reliable.

## Configuration model (single server/channel)
- Each user runs their own copy of the app and uses their own Discord bot token.
- The bot joins only the server and channel specified by `DISCORD_GUILD_ID` and `DISCORD_CHANNEL_ID`.
- To target a different server/channel, update those IDs and restart the app.

## Setup for your own server
1. Create a Discord application and bot at the Discord Developer Portal.
2. Build the app: `cargo build --release`
3. Run the setup wizard once: `target\\release\\discord-spotify-player.exe --setup`
4. Follow prompts to paste your token, choose a server, and choose a voice channel.
5. Start normally: `target\\release\\discord-spotify-player.exe`
6. Open Spotify on a device on the same network and select the new device in the Spotify Connect list.

**Note:** The bot routes audio to exactly one voice channel (the one in `DISCORD_CHANNEL_ID`). To use a different server/channel, change those IDs and restart.

## Compliance
- Not affiliated with Spotify or Discord.
- Intended for personal, non-commercial use; you are responsible for complying with Spotify's terms and applicable laws.
- Avoid using Spotify logos or implying endorsement in your own distributions.

## Privacy and data
- Spotify Connect credentials are cached locally in `.spotify_cache/credentials.json` after pairing.
- Environment variables live in `.env`; treat that file as sensitive.
- Logs are written locally only.

## Configuration inputs (high-level)
- Discord token and IDs for the target server and voice channel.
- A device name shown inside Spotify (default: `Discord Player`).
- Optional stable device ID to keep the device list clean.
- Audio buffer size (`AUDIO_BUFFER_SECONDS`, default 8) and prebuffer time (`PREBUFFER_SECONDS`, default 2.0).
- Optional EQ: `PREAMP_DB`, `BASS_BOOST_DB`, `TREBLE_BOOST_DB` (all default 0.0).
- Logging level via `RUST_LOG` (default `warn`). Simple values (`trace`, `debug`, `info`, `warn`, `error`) use app-centric presets that keep dependency logs quiet. Custom `RUST_LOG` filter strings are also accepted.

## Logging and troubleshooting
- By default logs stay at `warn` for all crates, so output is minimal.
- Set `RUST_LOG=debug` for more detail or `RUST_LOG=trace` for full diagnostics. These still only increase verbosity for this app, not dependencies.
- For full control pass a custom filter: `RUST_LOG="debug,librespot=info"`.
- Audio pipeline stats are emitted at `debug` level every 5 seconds on the `audio_stream` target.

## Further reading
- `docs/components.md` for a component overview.

## Typical experience
- You start the bot and it appears in the Spotify device list.
- Selecting it redirects playback to the Discord voice channel.
- Everyone in the call hears the Spotify audio.
