# AGENTS.md

A Discord music bot: per-user Spotify (via librespot + OAuth device
authorization), plus YouTube/SoundCloud/files, streamed into one voice
channel. This repo is the hardened reference for nob's music stack — see
`docs/PORT.md` before large changes. Public docs: `docs/`; file map:
`CODEMAP.md`; release notes: `CHANGELOG.md`. Working files (audits, plans)
go in the gitignored `.local/`, not `docs/`.

## Safety and secrets
- Never print or paste values from `.env`, `spotibot.db`, or `.spotify_cache/`.
- Keep `.env`, `spotibot.db*`, `.user_creds*` local-only (all gitignored).
- No user-specific identifiers or tokens in code or docs.
- Prefer documenting settings in `.env.example`.

## Build and run
- `cargo build --release`; binary is `target\release\discord-spotify-player.exe`.
- `cargo check` for fast feedback; `cargo test`; `cargo clippy --all-targets -- -D warnings` (what CI runs).
- Stop a running bot before `cargo build --release` — it locks the exe.
- First-run setup: `--setup` writes `.env`. OAuth needs no config.
- `.cargo/config.toml` (tracked) carries the cmake fix; MSVC toolchain on Windows.

## Architecture (see `docs/architecture.md`, `CODEMAP.md`)
- Two audio producers push PCM f32 into `AudioBridge`; `SimpleBridgeReader`
  pulls it out for Songbird. One player actor (`player/actor.rs`, pure
  decision core in `player/state.rs`) owns the queue, the armed Spotify
  track, and the turn — who's entitled to be audible. Priority: DJ overlay >
  the queue (Spotify tracks, YT/SC, files — same true order as `/queue`
  lists, radio rules: the bot never skips a track on its own) > Spotify
  Connect baseline. While Spotify holds the turn, the actor arms the first
  Spotify track anywhere in the queue into Spotify's own queue, so
  librespot's own track-end advance lands on it once any media items ahead
  of it have played.
- The Spotify session is its own lifecycle (`SessionSupervisor` in
  `spotify/session.rs`), OAuth-only (discovery/mDNS was removed in v0.5),
  background, and structurally unable to reach playback — it imports neither
  songbird nor the queue. One active DJ at a time; auto-start replays the
  stored active user on boot.
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
- Controlling playback (buttons, `/play`, `/queue`, `/skip`, `/stop`, session
  takeover) requires sharing the bot's voice channel. Exceptions: `/play` with
  the bot out of voice needs only *some* voice channel (the bot follows the
  requester in); `/announce` is a guild-level toggle, not gated.

## Testing
- Pure logic is unit-tested (parsers, crypto, store, biquads, ring buffer).
  Add tests for new pure units; write them nob-style so they port.

## Librespot notes
- Requires Spotify Premium. Reverse-engineered protocol (Spotify ToS gray
  area); no DRM bypass.
