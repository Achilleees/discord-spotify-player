# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository.

## What This Is

A Rust application that creates a Spotify Connect device and streams its audio into a Discord voice channel. No text commands are required; control happens from Spotify clients. It runs locally with one bot per server/channel.

## Build and Run

```bash
cargo build --release
target/release/discord-spotify-player.exe
```

For first-time setup (or to reconfigure), run:

```bash
target/release/discord-spotify-player.exe --setup
```

Use `cargo check` for fast compile-error feedback without a full build. There are no tests yet.

### Build Prerequisites

- MSVC toolchain (Visual Studio C++ build tools), required by native deps (opus, cmake)
- `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` to fix cmake builds
- `vergen = "=9.0.6"` and `vergen-gitcl = "=1.0.5"` are pinned in build-dependencies to work around a librespot version conflict ([upstream issue](https://github.com/librespot-org/librespot/issues/1681))

## Configuration

Config is loaded from `.env` (see `.env.example`). Required keys are `DISCORD_TOKEN`, `DISCORD_GUILD_ID`, and `DISCORD_CHANNEL_ID`.

Startup behavior:
- `--setup` runs the interactive setup wizard
- without `--setup`, the app tries `.env` first
- if `.env` is missing/invalid, setup wizard is launched automatically

Logging is controlled by `RUST_LOG`:
- simple values (`trace`, `debug`, `info`, `warn`, `error`) use app-centric presets (this crate at that level, dependencies at `warn`)
- any other value is treated as a raw `EnvFilter` string
- default (no `RUST_LOG`) is `warn` for all crates, so only `println!` app messages appear

## Architecture

### Audio Pipeline (core data flow)

```
Spotify (librespot decode) -> DiscordSink -> AudioBridge -> SimpleBridgeReader -> Songbird -> Discord voice
```

- **DiscordSink** (`src/spotify/sink.rs`): librespot audio backend. Receives decoded f64 samples, converts to f32, applies optional DSP (preamp + biquad EQ in frame-based stereo pairs), paces output to real-time, and pushes into the bridge. This is the hot audio path; avoid allocations and heavy work.
- **AudioBridge** (`src/audio_bridge.rs`): lock-based `VecDeque<f32>` ring buffer shared between producer (Spotify) and consumer (Discord). Drops samples when full. Uses bulk `as_slices()` copy on the consumer side. Audio stays at 44.1kHz stereo; Songbird handles resampling to 48kHz.
- **SimpleBridgeReader** (`src/discord/voice.rs`): implements `Read + Seek + MediaSource` for Songbird. Pulls from the bridge, does prebuffering on first read, and sleeps when empty to pace Songbird.

### Startup Sequence (`src/main.rs`)

1. Initialize logging from `RUST_LOG`
2. Load config from `.env`, or run setup wizard (`--setup` or missing/invalid config)
3. Create `AudioBridge`
4. Start Discord bot, join target voice channel, and wait for ready signal
5. Run Spotify Connect discovery loop (blocks forever, reconnects on disconnect)

### Spotify Connect Session (`src/spotify/player.rs`)

`SpotifyPlayer::run_discovery` is the main loop. It:
- announces a Spotify Connect device via mDNS discovery
- accepts credentials (from discovery or cache)
- creates librespot `Session`, `Player`, and `Spirc` (Spotify Connect controller)
- monitors `PlayerEvent`s to update Discord presence and clear audio buffer on pause/stop
- auto-reconnects with exponential backoff (up to `MAX_CACHED_RECONNECTS`)

Device ID is resolved from `DEVICE_ID` env var -> cached file -> random generation to keep the Spotify device list clean.

### Discord Presence (`src/presence.rs`, `src/discord/presence.rs`)

`PresenceUpdate` flows from Spotify player events -> mpsc channel -> `run_presence_loop`. The bot custom status shows the current track or idle/paused state.

## Key Crate Versions

- `librespot 0.8` (includes keepalive fix for stable connections)
- `serenity 0.12` + `songbird 0.5` (Discord gateway and voice)
- audio format: 44.1kHz stereo f32 through the pipeline

## Safety and Secrets

- Never print or commit values from `.env` or `.spotify_cache/`
- `.spotify_cache/credentials.json` contains Spotify auth tokens and must stay local-only
- Prefer documenting settings in `.env.example`

## Roadmap Branches

- `feat/now-playing-channel`: planned, rich embeds and playback control buttons in a text channel
- `feat/setup-wizard`: complete on branch, adds interactive CLI first-run config
- `feat/youtube-support`: planned, YouTube audio via `yt-dlp` alongside Spotify
