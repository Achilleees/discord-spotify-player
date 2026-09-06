# AGENTS.md

A Discord music bot: per-user Spotify (via librespot + OAuth device
authorization), plus YouTube/SoundCloud/files, streamed into one voice
channel. This repo is the continuing foundation for Spotibot and nob: one
workspace, shared music code, two separate bot processes. Nob features are
being brought here; the eventual project name is `never-off-beat`. See
`docs/PORT.md` for the accepted direction before large changes. Public docs:
`docs/`; file map: `CODEMAP.md`; release notes: `CHANGELOG.md`. Working files (audits, plans)
go in the gitignored `.local/`, not `docs/`.

## Safety and secrets
- Never print or paste values from `.env`, `spotibot.db`, or `.spotify_cache/`.
- Keep `.env`, `spotibot.db*`, `.user_creds*` local-only (all gitignored).
- No user-specific identifiers or tokens in code or docs.
- Prefer documenting settings in `.env.example`.

## Git workflow
- `dev` is the normal work and integration branch. Routine work is committed
  and pushed directly there; this solo repository does not use pull requests.
- `main` is deployment-only: every push rebuilds and restarts the VPS service.
  Do not update or push `main` without explicit deployment intent.
- Promote only an already-green `dev` commit to `main`, using a fast-forward,
  so the deployed SHA is exactly the SHA CI validated.

## Work tracking
- Work lives on Bef's board under project `discord-spotify-player`, exposed by
  the local `bef` MCP server as `mcp__bef__progress_*`. Keep its credential in
  local agent configuration only; never commit it.
- At session start call `progress_snapshot` for this project. Use
  `progress_list`/`progress_get` for detail, and file agreed work with
  `progress_create` before finishing the turn.
- Advance or close work with `progress_update`. Keep the project headline and
  focus current with `progress_headline`/`progress_focus`; their
  `expected_revision` is the project revision, not a task version.
- Pins are durable priority (`progress_pin`, `progress_unpin`,
  `progress_pins_order`). `progress_note` is reserved for Achille's steering.

## Build and run
- `cargo build --release`; binary is `target\release\discord-spotify-player.exe`.
- `cargo check --workspace --locked` for fast feedback; CI runs
  `cargo test --workspace --locked`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`, and
  `cargo build --workspace --release --locked`.
- Stop a running bot before `cargo build --release` — it locks the exe.
- First-run Spotibot setup: `--setup` writes `.env`. Nob uses `.env.nob.example`.
- Both hosts support `--env-file PATH` and offline `--check-config`.
- Nob-only server commands enforce caller and bot permissions at invocation.
- `.cargo/config.toml` (tracked) carries the cmake fix; MSVC toolchain on Windows.

## Architecture (see `docs/architecture.md`, `CODEMAP.md`)
- The root package remains the default workspace member and Spotibot host;
  `crates/nob` calls `run_nob()` in the same library as a separate process.
  `runtime.rs` owns profile config, frozen paths and process-held state locks.
  Nob uses `.env.nob` / `NOB_*` variables and `.nob` state by default; never
  introduce a fallback to Spotibot credentials or shared writable caches.
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
- Controlling playback requires sharing the bot's voice channel. `/play`,
  Add music and idle Play allow a requester in any voice channel while the
  bot is out of voice. `/clear` has that same exception after a stop. Queue
  and History inspection buttons are private reads; `/announce` is an ungated
  guild setting. Recheck voice after slow lookups and on result selection.

## Testing
- Pure logic is unit-tested (parsers, crypto, store, biquads, ring buffer).
  Add tests for new pure units; write them nob-style so they port.

## Librespot notes
- Requires Spotify Premium. Reverse-engineered protocol (Spotify ToS gray
  area); no DRM bypass.
