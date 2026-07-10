# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What This Is

A Rust Discord music bot (v0.5). It runs a per-user Spotify Connect session
(librespot + OAuth PKCE) and also plays YouTube/SoundCloud/uploaded files, all
streamed into one Discord voice channel. Control happens from Spotify clients,
from Discord slash commands (`/login`, `/queue`, `/play`, `/skip`, `/stop`,
`/who`, `/np`, `/announce`, `/logout`, `/forget`), and from now-playing buttons.

This repo is the hardened reference for the music stack of `never-off-beat`
(nob). **Read `PORT.md`** before large changes — it maps modules to nob and
records the design decisions.

## Build and Run

```
cargo build --release
target/release/discord-spotify-player.exe          # normal
target/release/discord-spotify-player.exe --setup  # first-run wizard
```

`cargo check` for fast feedback, `cargo test` (48 unit tests), `cargo clippy`.

### Prerequisites
- MSVC toolchain (native deps: opus, cmake). `.cargo/config.toml` (tracked)
  sets `CMAKE_POLICY_VERSION_MINIMUM=3.5`.
- `vergen = "=9.0.6"` / `vergen-gitcl = "=1.0.5"` pinned in build-deps
  (librespot#1681).
- `yt-dlp` + `ffmpeg` on `PATH` for `/play` (optional).

## Configuration

`.env` (see `.env.example`). Required: `DISCORD_TOKEN`, `DISCORD_GUILD_ID`,
`DISCORD_CHANNEL_ID`, `SPOTIFY_CLIENT_ID`. Recommended: `TOKEN_ENC_KEY`
(encrypts stored tokens), `TEXT_CHANNEL_ID`. Optional: `AUDIO_BUFFER_SECONDS`,
`PREBUFFER_SECONDS`, `PREAMP_DB`, `BASS_BOOST_DB`, `TREBLE_BOOST_DB`,
`DEVICE_NAME`, `DEVICE_ID`, `SPOTIBOT_DB`, `RUST_LOG`.

`--setup` runs the wizard; otherwise the app loads `.env` and errors if
`SPOTIFY_CLIENT_ID` is missing (OAuth is the only session path).

`RUST_LOG`: a preset (`trace|debug|info|warn|error`, app-centric) or a raw
`EnvFilter`. Default `warn`.

## Architecture

### Audio pipeline
```
Spotify / YouTube / files / DJ ─> AudioBridge ─> SimpleBridgeReader ─> Songbird ─> Discord
```
- **DiscordSink** (`src/spotify/sink.rs`): librespot backend; DSP + real-time
  pacing; pushes into the bridge. Hot path.
- **AudioBridge** (`src/audio_bridge.rs`): `VecDeque<f32>` ring buffer, 44.1 kHz
  stereo; drains/drops on even stereo frames.
- **SimpleBridgeReader** (`src/discord/voice.rs`): Songbird source; prebuffers
  per `PREBUFFER_SECONDS`.
- Priority: DJ overlay > queue (YT/SC/files) > Spotify Connect baseline
  (`src/queue.rs` + the priority-queue manager in `bot.rs`).

### Startup (`src/main.rs`)
1. Init logging from `RUST_LOG`.
2. Load config (or run wizard); build the OAuth client (requires
   `SPOTIFY_CLIENT_ID`).
3. Open the SQLite credential store (`spotibot.db`).
4. Create `AudioBridge`; start the Discord bot; wait for ready.
5. `ready()` (first time only) cleans stale controls and auto-starts the stored
   active user's session; `main` then parks. New sessions start via `/login`.

### Sessions (`src/discord/bot.rs`, `src/spotify/player.rs`)
- `spawn_session` joins voice, posts controls, starts the priority-queue
  manager, spawns the librespot task (`run_with_token`) and a proactive
  token-refresher (single owner of the refresh cycle). One active DJ; takeover
  requires being in the bot's voice channel.

### OAuth + storage
- `src/oauth/mod.rs`: Authorization Code + PKCE, paste-back parsing, refresh.
- `src/users/mod.rs` + `crypto.rs`: SQLite `spotify_credentials`, encrypted
  `auth_blob` (XChaCha20-Poly1305).

### Presence (`src/presence.rs`, `src/discord/presence.rs`)
- Player events → `PresenceUpdate` (carries `track_id` + `access_token`) →
  `run_presence_loop_with_track` → bot status + now-playing embeds.

## Key crate versions
- serenity 0.12, songbird 0.6 (native DAVE — not the git fork).
- librespot 0.8 (core/connect/playback/metadata; discovery removed).
- rusqlite 0.32 (bundled), sha2 + chacha20poly1305 (free via songbird's DAVE).

## Safety and secrets
- Never print or commit `.env`, `spotibot.db*`, `.user_creds*`, `.spotify_cache/`
  (all gitignored). No user-specific identifiers in code or docs.
