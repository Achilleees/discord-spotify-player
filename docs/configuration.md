# Configuration

Spotibot loads `.env` from the process working directory; unprefixed process
variables override it. Nob loads `.env.nob` and accepts only `NOB_*` process
overrides (`NOB_DISCORD_TOKEN`, `NOB_STATE_DIR`, etc.). File keys are unprefixed
for both profiles. Nob never falls back to Spotibot's `.env` or unprefixed
process settings. Use literal credential values in the selected file; dotenv
variable interpolation explicitly references process variables.

`--env-file PATH` selects a different file without loading a default file or
searching parent directories. `--check-config` validates required settings
and resolves paths without connecting, probing tools or writing state. It
does not authenticate credentials or test filesystem access. `--help` needs
no configuration. Only Spotibot's default launch offers the `.env` setup
wizard; nob and explicit-file/check launches fail on invalid configuration.

## Instance paths

| Setting | Spotibot default | Nob default |
|---|---|---|
| `STATE_DIR` | working directory, with legacy VPS media paths | `.nob` in working directory |
| `DATABASE_PATH` | `spotibot.db` | `nob.db` beneath state directory |
| `SPOTIFY_CACHE_DIR` | `.spotify_cache` | `.spotify_cache` beneath state directory |

With an explicit `STATE_DIR`, all relative paths below resolve beneath it,
including overrides. Absolute paths stay absolute. The state directory itself
is relative to the process working directory, not the env file. Setting a new
state directory selects new state; it does not migrate an existing database.
`SPOTIBOT_DB` remains a Spotibot-only legacy override relative to the working
directory; `DATABASE_PATH` takes precedence when both are set.

Nob, and Spotibot with explicit `STATE_DIR`, default to `youtube-tmp`,
`youtube-cookies.txt`, `dj-clips`, `dj-cache` and `kokoro.sock` under that state
directory. The tables below show Spotibot's legacy defaults when `STATE_DIR`
is absent. Writable database/cache directories must be accessible at startup;
file locks refuse concurrent use by another current host before cleanup,
including the writable YouTube cookie jar. yt-dlp uses `extractor-cache`
under its scratch directory and ignores ambient downloader configuration.
See [two-bots.md](two-bots.md) for running two identities.

## Discord (required)

| Variable | Required | Default | What it does |
|---|---|---|---|
| `DISCORD_TOKEN` | yes | — | Bot token from the Discord Developer Portal. |
| `DISCORD_GUILD_ID` | yes | — | The single guild (server) the bot operates in. Must be a non-zero Discord snowflake. |
| `DISCORD_CHANNEL_ID` | yes | — | The voice channel the bot joins and plays into. Must be a non-zero snowflake. |
| `TEXT_CHANNEL_ID` | no | the voice channel's built-in text chat | Text channel for the now-playing card and controls. An unset or invalid value falls back to the voice channel's chat. |

Missing or invalid required values prevent startup. The Spotibot default
launch can open its setup wizard; nob never opens or rewrites Spotibot config.

## Spotify session / storage

| Variable | Required | Default | What it does |
|---|---|---|---|
| `TOKEN_ENC_KEY` | no | unset | Any long random string, stretched via PBKDF2-HMAC-SHA256 (600,000 iterations, fixed app salt) into the XChaCha20-Poly1305 key that encrypts stored OAuth tokens at rest. If unset, tokens are stored **unencrypted**, with a startup warning. Strongly recommended on a shared or VPS host. |
| `SPOTIBOT_DB` | no | `spotibot.db` (cwd) | Path to the SQLite credential store. |
| `DEVICE_NAME` | no | `Discord Player` | Name shown in Spotify Connect's device list. |
| `DEVICE_ID` | no | auto-generated | Stable device id, to avoid Spotify creating a duplicate device entry across restarts. |

There is no Spotify developer app to register and no client secret — login
uses the device authorization grant against Spotify's own desktop client ID
(see `docs/getting-started.md`).

## Audio tuning

| Variable | Required | Default | Range | What it does |
|---|---|---|---|---|
| `AUDIO_BUFFER_SECONDS` | no | `8` | 1–12 | Size of the shared ring buffer (`AudioBridge`) in seconds of 44.1 kHz stereo audio. |
| `PREBUFFER_SECONDS` | no | `2.0` | 0.0–8.0 | How long `SimpleBridgeReader` waits for the buffer to fill on the very first read before handing audio to Songbird, so playback doesn't start on a starved buffer. |
| `PREAMP_DB` | no | `0.0` | -12–12 | Overall gain applied in `DiscordSink`'s DSP stage. |
| `BASS_BOOST_DB` | no | `0.0` | 0–12 | Low-shelf boost. |
| `TREBLE_BOOST_DB` | no | `0.0` | -6–6 | High-shelf boost/cut. |

Out-of-range values are clamped; an unparseable value is ignored with a
startup warning (`invalid numeric config value; using default`) rather than
silently defaulting, so a typo is visible in the logs.

## YouTube / SoundCloud / files

| Variable | Required | Default | What it does |
|---|---|---|---|
| `YOUTUBE_TMP_DIR` | no | `/tmp/spotibot-youtube` | Scratch directory for downloaded audio. Created on startup; swept of partial files left by a previous crash. |
| `YOUTUBE_COOKIES` | no | `/var/lib/spotibot/youtube-cookies.txt` | yt-dlp cookies file (Netscape format), used only if it exists on disk. Needed for age-restricted videos. |
| `YOUTUBE_MAX_DURATION_SECS` | no | `7200` (2h) | Maximum track length `/play`/`/queue` will accept for a YouTube/SoundCloud link. |

Both defaults above target the VPS layout; override them for a local run or
a different host layout.

## DJ (Kokoro TTS announcements)

| Variable | Required | Default | What it does |
|---|---|---|---|
| `DJ_CLIPS_DIR` | no | `/var/lib/spotibot/dj-clips` | Pre-recorded intro/outro clips, playable without a TTS backend. |
| `DJ_CACHE_DIR` | no | `/var/lib/spotibot/dj-cache` | FNV-hash cache of generated announcement audio (capped at 500 files). |
| `KOKORO_SOCKET` | no | `/var/lib/spotibot/kokoro.sock` | Unix domain socket for the Kokoro TTS backend. Unix-only; a non-Unix build stubs this out and only pre-recorded clips play. |

## Logging

| Variable | Required | Default | What it does |
|---|---|---|---|
| `RUST_LOG` | no | `warn` | Log level preset or a raw filter string (see below). |

### Presets vs raw filter

`RUST_LOG` accepts either a simple preset or a full `tracing-subscriber`
`EnvFilter` string:

- **Preset** — one of `trace`, `debug`, `info`, `warn`, `error` (case
  insensitive). This sets the app's own crates (`discord_spotify_player`,
  and the `audio_stream` and `player` targets) to that level, while
  `serenity`, `songbird`, and `librespot` stay at `warn` regardless — so
  `RUST_LOG=debug` gives you app-level detail without drowning in dependency
  chatter. Two additional noisy sub-targets are pinned to `error`:
  `librespot_connect::state::context` and `symphonia_bundle_mp3`.
- **Raw filter** — anything else is passed straight through as an
  `EnvFilter`, e.g. `RUST_LOG="debug,serenity=info,librespot=debug"`.

### The `player` and `audio_stream` targets

- `player` — every `Input` the player actor receives, and the resulting
  `active`/`sp`/`armed`/`device_active`/`queue_len`/`effects` after each
  `step`, at `debug`. This is the primary trace for "what did the player
  actor decide and why" — see `docs/troubleshooting.md`.
- `audio_stream` — buffer health (`buf_len`, pushed/pulled/dropped sample
  counts, last push/pull timestamps) logged every 5 seconds at `debug`.
  Useful for diagnosing audio dropouts or a starved bridge.

## Where state lives

- **Database** (`DATABASE_PATH`, legacy `SPOTIBOT_DB`, or the profile default) — SQLite database. Tables:
  `spotify_credentials` (per-user OAuth tokens, one row per Discord user,
  exactly one row `is_active = 1` at a time) and `settings` (bot-level
  toggles, e.g. the `/announce` state, so it survives restarts).
- **Token encryption** — the `auth_blob` column holds `access_token` +
  `refresh_token` as JSON, encrypted with XChaCha20-Poly1305 when
  `TOKEN_ENC_KEY` is set (AAD-bound to the owning Discord user id, so one
  user's blob can't be swapped onto another's row) or stored as plaintext
  JSON otherwise. A blob that fails to decrypt (wrong/rotated key, or
  corruption) is skipped with a warning rather than crashing the store.
