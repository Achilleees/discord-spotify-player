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
- No UI yet; configuration is via `.env`.

## Roadmap

Planned features (in progress):

| Feature | Branch | Status |
|---------|--------|--------|
| **Now Playing Channel** | `feat/now-playing-channel` | Planned |
| **Setup Wizard** | `feat/setup-wizard` | Planned |
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
2. Invite the bot to your server with permission to join/speak in voice.
3. Copy your server (guild) ID and the target voice channel ID.
4. Create a `.env` file based on `.env.example`.
5. Fill in:
   - `DISCORD_TOKEN`
   - `DISCORD_GUILD_ID`
   - `DISCORD_CHANNEL_ID`
   - Optional: `DEVICE_NAME`, `DEVICE_ID`, `RUST_LOG`
6. Start the bot:
   - Build and run: `cargo build --release` then `target\release\discord-spotify-player.exe`
7. Open Spotify on a device on the same network and select the new device in the Spotify Connect list.

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
- A device name shown inside Spotify.
- Optional stable device ID to keep the device list clean.
- Optional logging level via `RUST_LOG`.

## Further reading
- `docs/components.md` for a component overview.

## Typical experience
- You start the bot and it appears in the Spotify device list.
- Selecting it redirects playback to the Discord voice channel.
- Everyone in the call hears the Spotify audio.
