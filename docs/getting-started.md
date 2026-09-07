# Getting started

## Prerequisites

- Rust (stable) and the MSVC toolchain on Windows.
- `cmake` — needed to build native dependencies (opus, and librespot's audio
  backend). The tracked `.cargo/config.toml` sets
  `CMAKE_POLICY_VERSION_MINIMUM=3.5` so a modern cmake accepts opus's older
  build script.
- opus — built from source via the `audiopus_sys`/`opus` crates; no system
  package needed beyond cmake and a C compiler (MSVC on Windows).
- Optional: `yt-dlp` and `ffmpeg` on `PATH`. Without them, `/play` is
  disabled at registration time and only Spotify playback is available.

## Build

```
cargo build --release
```

The binary lands at `target/release/discord-spotify-player.exe` (Windows) or
`target/release/discord-spotify-player` (Linux).

Fast feedback while developing: `cargo check --workspace --locked`.
Run the CI checks across all workspace members with:

```
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
```

To build both hosts, use the workspace release command above. The second
binary is `target/release/nob` (`nob.exe` on Windows). Configure it using
`.env.nob.example`; see [running both bots](two-bots.md). Each process requires
a different Discord bot token and its own state.

## First run

Two ways to configure the bot:

1. **Setup wizard** — run with `--setup`:
   ```
   target/release/discord-spotify-player.exe --setup
   ```
   It prompts for a Discord bot token, lets you pick the server and voice
   channel from the bot's own guild list, and writes a `.env` file.
2. **Hand-written `.env`** — copy `.env.example` to `.env` and fill in the
   required values yourself (see `docs/configuration.md` for every variable).

If `.env` is missing or invalid on a normal launch, the bot prints the error
and falls back to the setup wizard automatically.

## Creating the Discord application

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications)
   and create a new application, then add a Bot to it.
2. Copy the bot token — this is `DISCORD_TOKEN`.
3. Under **Privileged Gateway Intents**, enable **Server Members Intent**:
   the bot uses `GUILDS`, `GUILD_VOICE_STATES`, and `GUILD_MEMBERS` (the
   last one is privileged; without it the voice-channel gates can't see who
   is in the call).
4. Invite the bot with the `bot` and `applications.commands` scopes. For
   ordinary voice playback and its cards, grant **View Channel**, **Send
   Messages**, **Embed Links**, **Read Message History**, **Connect** and
   **Speak** (permission integer `3230720`, as used by the setup wizard).
5. Right-click your server and voice channel (with Developer Mode enabled in
   Discord) to copy their IDs for `DISCORD_GUILD_ID` and
   `DISCORD_CHANNEL_ID`.

Check channel overrides too: voice access belongs on the voice channel, and
the message permissions belong on `DISCORD_TEXT_CHANNEL_ID` (or the voice
channel's text chat when that setting is omitted). See Discord's
[permission reference](https://docs.discord.com/developers/topics/permissions).
Music joins self-deafen through the gateway and need no **Deafen Members**
permission. Stage channels additionally need **Mute Members** to let the bot
[unsuppress itself](https://docs.discord.com/developers/resources/voice#modify-current-user-voice-state);
grant it on the stage channel if you use stages. Nob's server tools have
their own permission requirements in [two-bots.md](two-bots.md).

## Required environment variables

| Variable | What it's for |
|---|---|
| `DISCORD_TOKEN` | The bot token from the Developer Portal. |
| `DISCORD_GUILD_ID` | The server the bot operates in (single-guild bot). |
| `DISCORD_CHANNEL_ID` | The voice channel the bot joins and plays into. |

Everything else is optional — see `docs/configuration.md`. `TOKEN_ENC_KEY` is
strongly recommended on any shared or VPS host: without it, stored Spotify
tokens sit unencrypted in the SQLite database.

## First `/login`

No Spotify developer app or client secret is needed. Spotify login uses the
[device authorization grant](https://datatracker.ietf.org/doc/html/rfc8628)
against Spotify's own desktop client ID — the same one Spotify's official
clients use, which is what keeps playback working after Spotify's 2026-08
crackdown on third-party client IDs.

1. Run `/login` in Discord.
2. The bot replies with a link to `spotify.com/pair` and a short code.
3. Open that link on any device, log into Spotify, and enter the code.
4. Once approved, the bot polls Spotify until the login completes (up to 10
   minutes), then starts a Spotify Connect session.
5. Open Spotify on any device, tap the Connect (devices) icon, and pick the
   bot's device name (`DEVICE_NAME`, default "Discord Player") — it appears
   from anywhere, no shared network required.

Re-running `/login` later reuses the stored refresh token for a quick
reactivation with no new pairing. Taking over an active session someone else
owns requires being in the bot's voice channel.

## Running as a service on Linux

The reference deployment runs the bot as a dedicated system user under
`systemd`, with state under `/var/lib/spotibot` (the default for
`YOUTUBE_TMP_DIR`, `YOUTUBE_COOKIES`, `DJ_CLIPS_DIR`, `DJ_CACHE_DIR`, and
`KOKORO_SOCKET` all point there — see `docs/configuration.md`).

Layout:
- Binary: built with `cargo build --release` and copied into place, or built
  in-place from a checked-out repo.
- Env file: a `0600` file (e.g. `/etc/spotibot/env`) holding `DISCORD_TOKEN`,
  `TOKEN_ENC_KEY`, and the rest — loaded by the unit's `EnvironmentFile=`.
- State dir: `/var/lib/spotibot` owned by the service user, holding
  `spotibot.db` (`SPOTIBOT_DB`) and the YouTube/DJ scratch directories.

A systemd unit outline:

```ini
[Unit]
Description=Discord Spotify Player
After=network-online.target

[Service]
Type=simple
User=spotibot
WorkingDirectory=/var/lib/spotibot
EnvironmentFile=/etc/spotibot/env
ExecStart=/opt/spotibot/discord-spotify-player
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Keep the env file and the SQLite database owner-only (`chmod 600`); the bot
itself restricts the database file's permissions on Unix when it opens it.
Deploying by polling git for new commits and rebuilding on change (rather
than a push-based deploy) is a common pattern for a single-host setup like
this.
