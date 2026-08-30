# Discord Spotify Player (spotibot)

A personal Discord music bot. It runs a Spotify Connect device and streams its
audio into a voice channel, and also plays YouTube/SoundCloud and uploaded
files through the same pipeline. Per-user Spotify login happens from Discord;
playback is controlled from Spotify clients or from buttons in Discord.

> This repo is the hardened reference for the music stack of
> [never-off-beat](../never-off-beat) (nob). See `PORT.md`.

## Stack

- Discord gateway + voice: `serenity` 0.12, `songbird` 0.6 (native DAVE).
- Spotify Connect + playback: `librespot` pinned to the git `dev` branch
  (unreleased `add_to_queue` support; core/connect/playback/metadata).
- YouTube/SoundCloud/files: `yt-dlp` + `ffmpeg` (external binaries).
- Storage: SQLite (`rusqlite`, bundled). Runtime: `tokio`. Logs: `tracing`.

## What it does

- Per-user Spotify OAuth login via the `/login` slash command (device
  authorization grant — pair a code at spotify.com/pair, no app or client
  secret needed).
- Streams the logged-in user's Spotify playback into one voice channel; the bot
  follows the user into their channel.
- A now-playing text channel: rich embeds with album art, plus prev/pause/next
  buttons and a queue view.
- `/play`, `/queue` (Spotify, YouTube/SoundCloud/file — one shared queue,
  played in strict order like a radio), `/skip`, `/stop`, `/np`.
- Optional DJ track announcements via a Kokoro TTS backend, toggled with
  `/announce` (persists across restarts).
- Auto-starts the last active user's session on boot; auto-leaves and
  deactivates when the voice channel empties.

## Slash commands

| Command | What it does | Needs voice channel |
|---|---|---|
| `/login` | Start/re-activate a Spotify session; pair a code at spotify.com/pair to finish first-time auth | to take over another user's session |
| `/logout` | Stop and deactivate your session (credentials kept) | no |
| `/forget` | Delete your stored credentials | no |
| `/who` | Show the active DJ | no |
| `/play <url \| file> [next]` | Spotify/YouTube/SoundCloud URL or an audio attachment. Starts playback if nothing is playing; otherwise enqueues (`next:true` jumps the queue — behind an already-armed Spotify track if there is one, since Spotify can't be un-queued) | yes (see below) |
| `/queue [url \| file]` | Same as `/play` but always enqueues, never starts playback; with no argument, lists everything queued, in order | yes |
| `/skip` `/stop` | Skip / stop playback | yes |
| `/np` | Now playing (or "Paused" if the active session isn't playing) | no |
| `/announce` | Toggle DJ track announcements | no |

Playback control (buttons, `/play`, `/queue`, `/skip`, `/stop`) requires sharing
the bot's voice channel. Exception: when the bot isn't in voice yet, `/play`
only requires the requester to be in *some* voice channel — the bot joins them
(the fresh-boot path).

## How the queue works

`/play` and `/queue` share one bot-owned queue for Spotify tracks,
YouTube/SoundCloud links, and files. Tracks play strictly in that order,
regardless of source — like a radio. The bot never skips a track on its own;
`/queue` lists the true order, and the only way past a track is ⏭ or `/skip`.

- **While Spotify is playing:** the bot arms the *first Spotify track
  anywhere in the queue* into Spotify Connect's own queue. Spotify's own
  track-end advance then lands on that track — any YouTube/SoundCloud/file
  items ahead of it play first (Spotify sits paused on the armed track at
  0:00 in the meantime), then Spotify resumes onto it. The armed track is
  popped from the bot's queue once Spotify reports it playing. It's locked in
  once armed — Spotify can't be un-queued — so `next:true` can only insert
  behind it, and `/stop` clears everything else but that track still plays
  once.
- **A Spotify track starting mid-media** (e.g. picked in the Spotify app
  while a YouTube/SoundCloud/file item is playing) is paused immediately and
  resumes after the queue — Spotify never plays over the queue.
- **Spotify idle, Spotify track at the head:** the track is loaded directly
  with `load` (there's no context to preserve).
- **Spotify paused, Spotify track at the head:** the track is armed the same
  way, but queuing never resumes playback for you — press ▶ or ⏭ to hear it.
- **⏭ with a media track next:** the current Spotify track is also advanced
  (skipped), the media track plays, then Spotify resumes.
- A failed download removes its card and reposts the controls; the queue
  continues.
- The DJ's own phone-side Spotify queue is invisible to the bot — ordering
  between the two is best-effort.

## Requirements

- Spotify Premium (required for Spotify Connect playback).
- A Discord bot with the `GUILD_MEMBERS` intent, and permission to join/speak in
  a voice channel and to send/delete messages in the text channel.
- `yt-dlp` and `ffmpeg` on `PATH` for `/play` (optional; Spotify works without).

## Setup

1. `cargo build --release`
2. First run: `target/release/discord-spotify-player.exe --setup` (writes `.env`).
3. Add `TOKEN_ENC_KEY` to `.env` (see `.env.example`).
4. Start: `target/release/discord-spotify-player.exe`
5. In Discord, run `/login`, open the link to spotify.com/pair, and enter the
   code shown. The pair page shows the request as coming from Spotify's
   desktop app — that's expected, the bot authenticates with Spotify's own
   desktop client id.

## Configuration

See `.env.example`. Required: `DISCORD_TOKEN`, `DISCORD_GUILD_ID`,
`DISCORD_CHANNEL_ID`. Recommended: `TOKEN_ENC_KEY` (encrypts
stored tokens at rest), `TEXT_CHANNEL_ID` (now-playing channel; defaults to the
voice channel's text chat). Optional tuning: `AUDIO_BUFFER_SECONDS`,
`PREBUFFER_SECONDS`, `PREAMP_DB`, `BASS_BOOST_DB`, `TREBLE_BOOST_DB`,
`DEVICE_NAME`, `DEVICE_ID`, `RUST_LOG`, `SPOTIBOT_DB`, `YOUTUBE_COOKIES`,
`YOUTUBE_TMP_DIR`, `YOUTUBE_MAX_DURATION_SECS`, `DJ_CLIPS_DIR`, `DJ_CACHE_DIR`,
`KOKORO_SOCKET`.

## Logging

`RUST_LOG` accepts a preset (`trace|debug|info|warn|error`, app-centric — keeps
dependency logs quiet) or a raw `EnvFilter` string. Default is `warn`. Audio
pipeline stats emit at `debug` on the `audio_stream` target every 5s.

## Privacy and data

- Per-user OAuth tokens are stored in a local SQLite DB (`spotibot.db`),
  encrypted at rest when `TOKEN_ENC_KEY` is set. The DB and `.env` are local-only
  and gitignored.
- Not affiliated with Spotify or Discord. Personal, non-commercial use; you are
  responsible for complying with Spotify's terms.

## Development

`cargo check` for fast feedback, `cargo test` (186 unit tests), `cargo clippy`.
`.cargo/config.toml` carries a cmake fix required by native deps; the MSVC
toolchain is required on Windows.
