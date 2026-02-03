# Discord Spotify Player (Pre-release) — Quick Start

This zip contains a Windows build of the Discord Spotify Player.

## Requirements
- Spotify Premium account.
- A Discord bot token with permission to join and speak in a voice channel.
- The computer running the app should be on the same network as your Spotify client.

## Setup
1. Extract the zip contents to a folder.
2. Copy `.env.example` to `.env`.
3. Edit `.env` and set:
   - `DISCORD_TOKEN`
   - `DISCORD_GUILD_ID`
   - `DISCORD_CHANNEL_ID`
4. Optional:
   - `DEVICE_NAME` to change how the device appears in Spotify.
   - `DEVICE_ID` to keep the device list stable across runs.
   - `RUST_LOG=info` (or `debug`) for extra logs.

## Run
- Double-click `discord-spotify-player.exe`, or
- Run from terminal: `discord-spotify-player.exe`

## Use
1. Open Spotify on a device on the same network.
2. Select the device name in the Spotify Connect list.
3. Playback will stream into the configured Discord voice channel.

## Notes
- This runs locally and connects a single bot to a single server/channel per run.
- To change servers/channels, update `.env` and restart the app.
- Not affiliated with Spotify or Discord. Intended for personal, non-commercial use.
