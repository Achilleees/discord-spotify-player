# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What This Is

A Rust Discord music bot (v0.5). It runs a per-user Spotify Connect session
(librespot + OAuth device authorization) and also plays YouTube/SoundCloud/uploaded files, all
streamed into one Discord voice channel. Control happens from Spotify clients,
from Discord slash commands (`/login`, `/play`, `/queue`, `/skip`, `/stop`,
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

`cargo check` for fast feedback, `cargo test`, `cargo clippy`. Stop the
running bot before `cargo build --release` — it locks the exe.

### Prerequisites
- MSVC toolchain (native deps: opus, cmake). `.cargo/config.toml` (tracked)
  sets `CMAKE_POLICY_VERSION_MINIMUM=3.5`.
- `yt-dlp` + `ffmpeg` on `PATH` for `/play` (optional).

## Configuration

`.env` — every variable is documented in `.env.example`; path defaults target
the VPS layout under `/var/lib/spotibot`. `--setup` runs the wizard;
otherwise the app loads `.env`. OAuth needs no config — it authenticates
against Spotify's desktop client id.

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
  stereo; drains/drops on even stereo frames. Only the turn holder may clear
  it — the Spotify layer never does.
- **SimpleBridgeReader** (`src/discord/voice.rs`): Songbird source; prebuffers
  per `PREBUFFER_SECONDS`.
- Priority: DJ overlay > the player's queue (Spotify tracks, YT/SC, files, in
  true order — radio rules, the bot never skips on its own) > Spotify Connect
  baseline. The player actor owns the "turn" — who is entitled to be audible
  — and decides this ordering (`src/queue.rs`, `src/player/`).

### Player actor (`src/player/state.rs`, `src/player/actor.rs`)
- **One player actor owns all playback state**: the queue, the armed Spotify
  track, and the turn. Every command (slash commands, buttons, the Spotify
  session, timers) reaches it as an `Input` through a mailbox; the actor
  awaits nothing, ever — every effect it produces is a synchronous send or a
  `tokio::spawn` (media runners, voice joins, announcements, timers).
- The decision logic is a **pure core**, `src/player/state.rs`:
  `step(state, input, now) -> Vec<Effect>`. It imports no serenity, songbird
  or librespot-connect types — only `std`, a plain `oneshot` handle,
  `SpotifyUri` and `crate::queue` — so it's deterministic under test and is
  the piece that ports to nob unchanged (74 of the crate's tests).
- **Radio rules**: tracks play strictly in queue order regardless of source;
  the bot never sends Spotify a `Next` on its own — only a human skip
  (⏭/`/skip`) does. While Spotify holds (or is about to hold) the turn, the
  actor arms the first Spotify track anywhere in the queue into Spotify's own
  queue (`SpircCommand::AddToQueue`), so librespot's own track-end advance
  lands on it; any queue items ahead of it play first, then Spotify resumes
  onto the armed track.
- `/np` and the queue listing read a `PlayerSnapshot` straight from the actor
  (`PlayerHandle::query`) rather than a cached copy.

### Spotify session (`src/spotify/session.rs`, `src/spotify/player.rs`)
- `SessionSupervisor` owns the Spotify session's own lifecycle — the
  librespot task, its proactive token refresher, and the session generation.
  It's background: started by `/login`, boot auto-start, or on demand via
  `ensure_session` when a Spotify link is queued with the link down.
  **It imports neither songbird nor the queue**, so it is structurally
  unable to reach playback; its only surface into the player is `Input`,
  delivered through the same mailbox as every other source.
- `run_with_token` (`src/spotify/player.rs`) drives the Spirc session
  lifecycle and applies `SpircCommand` (`Play`/`Pause`/`Next`/`Previous`/
  `AddToQueue`/`Load`/`Lookup`/`ActivateDevice`/`Transfer`) to the live Spirc
  — no calls to api.spotify.com. `Lookup` resolves title/artist/album art for
  a Spotify item at enqueue time (`Track::get`), so the queue never needs the
  Web API either.
- **Account** is a third, independent lifecycle: `/login` stores credentials,
  marks the account active, and calls `SessionSupervisor::switch` — nothing
  else. One active DJ at a time; takeover requires being in the bot's voice
  channel.

### UI (`src/discord/ui.rs`)
- One task owns the single now-playing/controls card, keyed on one `card_id`.
  Both the Spotify baseline and the queue post through its mailbox (`UiMsg`)
  rather than touching the channel directly, so the two playback sources can
  never race each other's post/delete.

## Dependency gotchas
- songbird is the crates.io release with native DAVE — not the git fork.
- librespot is pinned to git `dev` (rev `1599145`, 2026-08-22) for the
  unreleased `add_to_queue` — bump to the next crates.io release once it ships.

## Safety and secrets
- Never print or commit `.env`, `spotibot.db*`, `.user_creds*`, `.spotify_cache/`
  (all gitignored). No user-specific identifiers in code or docs.
