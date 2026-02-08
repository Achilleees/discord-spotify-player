# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

A Rust application that creates a Spotify Connect device and streams its audio into a Discord voice channel. No text commands — control happens entirely from Spotify clients. Runs locally, one bot per server/channel.

## Build and Run

```bash
cargo build --release
target/release/discord-spotify-player.exe
```

Use `cargo check` for fast compile-error feedback without a full build. There are no tests yet.

### Build Prerequisites

- MSVC toolchain (Visual Studio C++ build tools) — required by native deps (opus, cmake)
- `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` to fix cmake builds
- `vergen = "=9.0.6"` and `vergen-gitcl = "=1.0.5"` are pinned in build-dependencies to work around a librespot version conflict ([upstream issue](https://github.com/librespot-org/librespot/issues/1681))

## Configuration

All config is via `.env` (see `.env.example`). Required: `DISCORD_TOKEN`, `DISCORD_GUILD_ID`, `DISCORD_CHANNEL_ID`. Logging is controlled by `RUST_LOG` — simple levels (`trace`, `debug`, `info`, `warn`, `error`) use app-centric presets where this crate gets that level and dependency crates stay at `warn`. Any other value is passed through as a raw `EnvFilter` string for full control. Default (no `RUST_LOG` set) is app=`info`, deps=`warn`.

## Architecture

### Audio Pipeline (the core data flow)

```
Spotify (librespot decode) → DiscordSink → AudioBridge → SimpleBridgeReader → Songbird → Discord voice
```

- **DiscordSink** (`src/spotify/sink.rs`): Librespot audio backend. Receives decoded f64 samples, converts to f32, applies optional DSP (preamp + biquad EQ in frame-based stereo pairs), paces output to real-time, and pushes into the bridge. This is the hot audio path — avoid allocations and heavy work here.
- **AudioBridge** (`src/audio_bridge.rs`): Lock-based `VecDeque<f32>` ring buffer shared between producer (Spotify) and consumer (Discord). Drops samples when full. Uses bulk `as_slices()` copy on the consumer side. All audio stays at 44.1kHz stereo; Songbird handles resampling to 48kHz.
- **SimpleBridgeReader** (`src/discord/voice.rs`): Implements `Read + Seek + MediaSource` for Songbird. Pulls from the bridge, does prebuffering on first read, and sleeps when empty to pace Songbird.

### Startup Sequence (main.rs)

1. Load config from `.env`
2. Create `AudioBridge`
3. Start Discord bot → join voice channel → signal ready via oneshot
4. Wait for Discord ready
5. Run Spotify Connect discovery loop (blocks forever, reconnects on disconnect)

### Spotify Connect Session (`src/spotify/player.rs`)

`SpotifyPlayer::run_discovery` is the main loop. It:
- Announces a Spotify Connect device via mDNS discovery
- Accepts credentials (from discovery or cache)
- Creates a librespot `Session` + `Player` + `Spirc` (Spotify Connect controller)
- Monitors `PlayerEvent`s to update Discord presence and clear the audio buffer on pause/stop
- Auto-reconnects with exponential backoff (up to `MAX_CACHED_RECONNECTS`)

Device ID is resolved from `DEVICE_ID` env var → cached file → random generation, to keep the Spotify device list clean.

### Discord Presence (`src/presence.rs`, `src/discord/presence.rs`)

`PresenceUpdate` enum flows from Spotify player events → mpsc channel → `run_presence_loop`. The bot's custom status shows the current track with alternating Unicode music notes, or idle/paused state.

## Key Crate Versions

- `librespot 0.8` — includes keepalive fix for stable connections
- `serenity 0.12` + `songbird 0.5` — Discord gateway and voice
- Audio format: 44.1kHz stereo f32 throughout the pipeline

## Safety and Secrets

- Never print or commit values from `.env` or `.spotify_cache/`
- `.spotify_cache/credentials.json` contains Spotify auth tokens — local only
- Prefer documenting settings in `.env.example`

## Roadmap Branches

Three planned feature branches (see AGENTS.md for implementation notes):
- `feat/now-playing-channel` — rich embeds + playback control buttons in a text channel
- `feat/setup-wizard` — interactive CLI for first-run config
- `feat/youtube-support` — YouTube audio via yt-dlp alongside Spotify
