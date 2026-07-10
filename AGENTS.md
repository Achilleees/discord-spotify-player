# AGENTS.md

A Discord music bot: per-user Spotify (via librespot + OAuth PKCE), plus
YouTube/SoundCloud/files, streamed into one voice channel. This repo is the
hardened reference for nob's music stack — see `PORT.md` before large changes.

## Safety and secrets
- Never print or paste values from `.env`, `spotibot.db`, or `.spotify_cache/`.
- Keep `.env`, `spotibot.db*`, `.user_creds*` local-only (all gitignored).
- No user-specific identifiers or tokens in code or docs.
- Prefer documenting settings in `.env.example`.

## Build and run
- `cargo build --release`; binary is `target\release\discord-spotify-player.exe`.
- `cargo check` for fast feedback; `cargo test` (48 unit tests); `cargo clippy`.
- First-run setup: `--setup` writes `.env`. OAuth also needs `SPOTIFY_CLIENT_ID`.
- `.cargo/config.toml` (tracked) carries the cmake fix; MSVC toolchain on Windows.

## Architecture (see `docs/components.md`)
- Two audio producers push PCM f32 into `AudioBridge`; `SimpleBridgeReader`
  pulls it out for Songbird. Priority: DJ overlay > queue (YT/SC/files) >
  Spotify Connect baseline.
- Sessions are OAuth-only (discovery/mDNS was removed in v0.5). One active DJ at
  a time; auto-start replays the stored active user on boot.
- Tokens live in SQLite (`spotify_credentials`, encrypted `auth_blob`). One
  proactive refresher task owns the refresh cycle.

## Audio/perf
- No allocations or heavy logging in the hot path (`sink.rs`, `audio_bridge.rs`).
- Real-time pacing lives in `DiscordSink::write`, not the reader.
- The ring buffer drops/drains on even (stereo-frame) boundaries — keep it so.

## Logging policy
- Lowercase messages; structured fields over format strings
  (`tracing::debug!(samples = n, "push_samples")`).
- Sink start/stop are `debug`. Reserve `info` for startup/connection milestones.
- High-frequency audio diagnostics go on the `audio_stream` target at `debug`.

## Dependency policy
- serenity 0.12, songbird 0.6 (native DAVE — do NOT reintroduce the git fork).
- librespot 0.8 with the pinned `vergen`/`vergen-gitcl` build-dep workaround
  (upstream issue librespot#1681). Check upstream before bumping.
- `rand` 0.10 (`rand::random::<T>()`). `parking_lot::Mutex` for the audio hot
  path; `std::sync::Mutex` elsewhere is fine.
- `sha2`/`chacha20poly1305` come free via songbird's DAVE — reuse, don't add
  crypto crates.

## Authorization
- Controlling playback (buttons, `/queue`, `/play`, `/skip`, `/stop`,
  `/announce`, session takeover) requires sharing the bot's voice channel.

## Testing
- Pure logic is unit-tested (parsers, crypto, store, biquads, ring buffer).
  Add tests for new pure units; write them nob-style so they port.

## Librespot notes
- 0.8.0 includes the keepalive fix (PR #1359). Requires Spotify Premium.
  Reverse-engineered protocol (Spotify ToS gray area); no DRM bypass.
