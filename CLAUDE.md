# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What This Is

A Rust Discord music bot (v0.5). It runs a per-user Spotify Connect session
(librespot + OAuth device authorization) and also plays YouTube/SoundCloud/uploaded files, all
streamed into one Discord voice channel. Control happens from Spotify clients,
from Discord slash commands (`/login`, `/play`, `/queue`, `/skip`, `/stop`,
`/who`, `/np`, `/announce`, `/logout`, `/forget`), and from now-playing buttons.

This repo is the continuing foundation for Spotibot and nob: one workspace,
shared music code, two separately configured bot processes. Nob's useful
features are being brought here; the eventual project name is `never-off-beat`.
**Read `docs/PORT.md`** before large changes — it records the accepted direction
and clearly separates the superseded transfer dossier. Public docs live in
`docs/`, the file map in `CODEMAP.md`; release notes go in `CHANGELOG.md` (prose per release, one
bold headline per user-visible change). Working files (audits, plans) go in
the gitignored `.local/`, never in `docs/`.

## Git Workflow

- `dev` is the normal work and integration branch. Routine work is committed
  and pushed directly there; this solo repository does not use pull requests.
- `main` is deployment-only: every push rebuilds and restarts the VPS service.
  Do not update or push `main` without explicit deployment intent.
- Promote only an already-green `dev` commit to `main`, using a fast-forward,
  so the deployed SHA is exactly the SHA CI validated.

## Build and Run

```
cargo build --release
target/release/discord-spotify-player.exe          # normal
target/release/discord-spotify-player.exe --setup  # first-run wizard
```

`cargo check --workspace --locked` for fast feedback. CI runs
`cargo test --workspace --locked`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, and
`cargo build --workspace --release --locked`. Stop the
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

The root package is the first Cargo workspace member. `src/main.rs` owns the
Tokio runtime and calls `discord_spotify_player::run()` in `src/lib.rs`, which
owns startup and the private runtime modules. The first imported UI adds
private search/link entry; nob's separate host and remaining features follow. Run each identity
in its own process with independent configuration, database and caches.

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
- That owner caches the desired card/account/pause state and refreshes every
  30 seconds, recreating only a confirmed missing message. Search menus live
  in `discord/search.rs`: bounded, private, owner/guild checked, expiring and
  single-use. Slash and menu track requests share `commands::add_track`,
  which rechecks voice after metadata work. `youtube/probe.rs` bounds both
  text search and link probes by concurrency, output size and time.

## Dependency gotchas
- songbird is the crates.io release with native DAVE — not the git fork.
- librespot is pinned to git `dev` (rev `1599145`, 2026-08-22) for the
  unreleased `add_to_queue` — bump to the next crates.io release once it ships.

## Safety and secrets
- Never print or commit `.env`, `spotibot.db*`, `.user_creds*`, `.spotify_cache/`
  (all gitignored). No user-specific identifiers in code or docs.

## Work tracking — Bef's board, project `discord-spotify-player`

Work for this repo lives on Bef's board (the Sidearm runtime on the VPS), reached
through the `bef` MCP server registered in each local agent client for this directory
(local-only config, never committed; `/track-project` registers Claude Code once per
checkout and worktree); its tools appear as `mcp__bef__progress_*`. Its bearer maps to
the runtime operator `discord-spotify-player`, which is scoped to this project and
nothing else. Every call names the project:

- `progress_snapshot {project: "discord-spotify-player"}` — counts and the ranked
  workset. Read it at session start.
- `progress_list {project: "discord-spotify-player", status: "open"}` — the open rows
  (filters: status, priority, kind, group, tag, query, limit; 50 per call).
- `progress_get {project: "discord-spotify-player", id}` — one row in full.
- `progress_create {project: "discord-spotify-player", title, context?, steps?, kind?, priority?, tags?}`
  — file work the moment it is agreed.
- `progress_update {project: "discord-spotify-player", id, expected_version?, status?, steps?, context_append?, …}`
  — advance or close a row; unknown keys and stale versions are refused.

- `progress_headline {project: "discord-spotify-player", expected_revision, text}` and
  `progress_focus {…}` — the board's own state: the headline is where the work stands,
  the focus is what the board is for. `expected_revision` is the PROJECT revision from
  `progress_snapshot`, never a task version.
- `progress_pin {project: "discord-spotify-player", id, expected_version, position, intent?}`,
  `progress_unpin` and `progress_pins_order` — a standing statement of priority that
  survives re-ranking.

Rules: file before you finish; one row per unit of work; `context` carries the why and
the numbers, never narration of the change. The headline is yours to write, so keep it
current and let this file and `docs/PORT.md` carry the longer narrative.
`progress_note` is the one board verb this session does not hold — it is Achille's own
lane for steering a row.
