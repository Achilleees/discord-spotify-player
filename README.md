# Discord Spotify Player (spotibot)

A personal Discord music bot. It runs a Spotify Connect device and streams its
audio into a voice channel, and also plays YouTube/SoundCloud and uploaded
files through the same pipeline. Per-user Spotify login happens from Discord;
playback is controlled from Spotify clients or from buttons in Discord.

> This repo is the hardened reference for the music stack of
> [never-off-beat](../never-off-beat) (nob). See `PORT.md`.

## Stack

- Discord gateway + voice: `serenity` 0.12, `songbird` 0.6 (native DAVE).
- Spotify Connect + playback: `librespot` 0.8 (core/connect/playback/metadata).
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
- `/queue`, `/play` (YouTube/SoundCloud/file), `/skip`, `/stop`, `/np`.
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
| `/queue <url>` | Add a Spotify track to the queue | yes |
| `/play <url>` | Queue a YouTube/SoundCloud/file track | yes (see below) |
| `/skip` `/stop` | Skip / stop playback | yes |
| `/np` | Now playing | no |
| `/announce` | Toggle DJ track announcements | no |

Playback control (buttons, `/queue`, `/play`, `/skip`, `/stop`) requires sharing
the bot's voice channel. Exception: when the bot isn't in voice yet, `/play`
only requires the requester to be in *some* voice channel — the bot joins them
(the fresh-boot path).

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

`cargo check` for fast feedback, `cargo test` (105 unit tests), `cargo clippy`.
`.cargo/config.toml` carries a cmake fix required by native deps; the MSVC
toolchain is required on Windows.
