# Components Overview

This document explains the main pieces of the app at a high level. It is intended for contributors; end users should start with README.md.

## Discord Voice Path (Serenity + Songbird)
- The bot logs in with your Discord bot token and connects to a single guild/channel.
- Songbird joins the target voice channel and plays a raw PCM stream.
- There are no text commands; the bot only handles voice.

## Spotify Connect Path (Librespot)
- The app exposes a Spotify Connect device via discovery on the local network.
- When you select the device, it pairs and starts a Spotify session.
- Spotify playback is decoded and pushed into the audio bridge.

## Audio Bridge and DSP
- A shared in-memory buffer bridges Spotify (producer) to Discord (consumer).
- Simple EQ controls exist today: preamp, bass boost, treble boost.
- The EQ runs in the audio sink; avoid heavy work in this path.

## Config and Cache
- Configuration is read from .env (see .env.example).
- Spotify credentials are cached locally in .spotify_cache/credentials.json.
- A stable device ID is used to avoid duplicate devices in Spotify.

## Presence and Logs
- The bot updates Discord presence with basic playback state.
- Logs are quiet by default; use RUST_LOG for troubleshooting.
