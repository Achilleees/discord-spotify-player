# AGENTS.md

A Discord music bot: per-user Spotify (via librespot + OAuth device
authorization), plus YouTube/SoundCloud/files, streamed into one voice
channel. This repo is the hardened reference for nob's music stack — see
`PORT.md` before large changes.

## Safety and secrets
- Never print or paste values from `.env`, `spotibot.db`, or `.spotify_cache/`.
- Keep `.env`, `spotibot.db*`, `.user_creds*` local-only (all gitignored).
- No user-specific identifiers or tokens in code or docs.
- Prefer documenting settings in `.env.example`.

## Build and run
- `cargo build --release`; binary is `target\release\discord-spotify-player.exe`.
- `cargo check` for fast feedback; `cargo test` (101 unit tests); `cargo clippy`.
- First-run setup: `--setup` writes `.env`. OAuth needs no config.
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
- librespot pinned to the git `dev` branch (unreleased `add_to_queue` and
  device-auth fixes). Bump to the next crates.io release (0.9+) once it ships.
- `rand` 0.10 (`rand::random::<T>()`). `parking_lot::Mutex` for the audio hot
  path; `std::sync::Mutex` elsewhere is fine.
- `sha2`/`chacha20poly1305` come free via songbird's DAVE — reuse them.
  `pbkdf2` 0.12 is the one direct crypto addition (TOKEN_ENC_KEY stretching);
  don't add others.

## Authorization
- Controlling playback (buttons, `/queue`, `/play`, `/skip`, `/stop`, session
  takeover) requires sharing the bot's voice channel. Exceptions: `/play` with
  the bot out of voice needs only *some* voice channel (the bot follows the
  requester in); `/announce` is a guild-level toggle, not gated.

## Testing
- Pure logic is unit-tested (parsers, crypto, store, biquads, ring buffer).
  Add tests for new pure units; write them nob-style so they port.

## Librespot notes
- Requires Spotify Premium. Reverse-engineered protocol (Spotify ToS gray
  area); no DRM bypass.
