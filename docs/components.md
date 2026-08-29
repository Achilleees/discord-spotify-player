# Components Overview

High-level map of the app for contributors; end users start with README.md.

## Audio pipeline (the core data flow)

```
Spotify (librespot decode) ─┐
YouTube/SoundCloud (yt-dlp) ─┼─> AudioBridge ─> SimpleBridgeReader ─> Songbird ─> Discord voice
uploaded files ─────────────┘         ▲
DJ TTS (Kokoro) ── overlay ───────────┘ (mixes on top at a fixed gain)
```

- **AudioBridge** (`src/audio_bridge.rs`): lock-based `VecDeque<f32>` ring buffer
  shared between producers and the consumer. 44.1 kHz stereo f32; Songbird
  resamples to 48 kHz. Drops on overflow; drains/drops on even stereo frames.
- **DiscordSink** (`src/spotify/sink.rs`): librespot's audio backend. Applies
  optional DSP (preamp + biquad low/high shelves) and paces output to real time,
  then pushes into the bridge. Hot path — no allocations or heavy logging.
- **SimpleBridgeReader** (`src/discord/voice.rs`): `Read + Seek + MediaSource`
  for Songbird. Prebuffers on first read (honors `PREBUFFER_SECONDS`), then
  pulls from the bridge.
- **Priority model** (`src/queue.rs`, managed in `bot.rs`): DJ overlay > one
  bot-owned queue (Spotify tracks and YouTube/SoundCloud/files, in the order
  they were added — `MediaSource::Spotify | YouTube | File`, YouTube covers
  SoundCloud via yt-dlp) > Spotify Connect baseline. A media item at the head never interrupts a
  playing Spotify track — the priority-queue manager waits for `EndOfTrack`
  before draining, then resumes Spotify afterwards only if it was playing
  before. A Spotify item at the head is instead pre-armed: while Spotify is
  playing, `try_arm_head` hands it to Spotify Connect's own queue
  (`AddToQueue`) so it plays gap-free at track end, tracked in
  `Handler.armed_spotify` and popped when the matching `Playing` event
  arrives; while Spotify is idle it's `Load`ed directly. `head_action` (pure
  fn) maps `(head kind, Spotify state, trigger)` to the action to take —
  arm, hand off, load, drain, or resume — and is unit-tested per case.

## Spotify path (librespot + OAuth)

- OAuth-only (device authorization grant, RFC 8628); mDNS discovery was
  removed in v0.5.
- `src/oauth/mod.rs`: `request_device_code`, `poll_device_token`, token
  refresh — all against Spotify's desktop client id. No profile fetch; the
  Spotify session's display name is the Discord display name.
  `src/spotify/player.rs`: `run_with_token` drives the Spirc
  session lifecycle (15s `Spirc::new` timeout, reconnect loop, event → presence).
  It also owns playback control: `SpircCommand`
  (`Play`/`Pause`/`Next`/`Previous`/`AddToQueue`/`Load`/`Lookup`) arrives over
  a channel and is applied to the live `Spirc` — no calls to api.spotify.com.
- Now-playing track metadata comes from librespot itself:
  `PlayerEvent::TrackChanged` carries the `AudioItem` (title, artist,
  track_id, album art), which feeds `PresenceUpdate` directly. Metadata for a
  Spotify item *queued* ahead of time comes from `SpircCommand::Lookup`
  (`Track::get(&session, &uri)` on the live session) instead — still no Web
  API call.
- Sessions: one active DJ; a proactive refresher task keeps the token fresh.

## Discord path (serenity + songbird)

- `src/discord/bot.rs`: gateway handler, slash commands (`/login`, `/logout`,
  `/forget`, `/who`, `/play`, `/queue`, `/skip`, `/stop`, `/np`, `/announce`),
  button interactions, now-playing/controls embeds, priority-queue manager, the
  voice-join + auto-leave logic, and `spawn_session`.
- `src/discord/voice.rs`: bridge reader + Songbird track events.
- `src/discord/presence.rs`: bot status text + presence loop.
- Controlling playback requires sharing the bot's voice channel. Exceptions:
  `/play` with the bot out of voice needs only some voice channel (the bot
  follows the requester in); `/announce` is an ungated guild-level toggle.

## Storage and config

- `src/users/mod.rs` + `crypto.rs`: per-user credentials in SQLite
  (`spotify_credentials`), tokens in an encrypted `auth_blob`
  (XChaCha20-Poly1305 with owner-bound AAD; key = PBKDF2-HMAC-SHA256 of
  `TOKEN_ENC_KEY`, 600k iterations, fixed app salt; plaintext with a warning if
  unset). A `settings` table persists bot-level toggles (e.g. `/announce`).
- `src/config.rs`: `.env` config (validated: non-zero snowflakes, warns on bad
  numbers). `src/setup.rs`: first-run CLI wizard.

## DJ / YouTube

- `src/audio/dj.rs`: Kokoro TTS client (Unix domain socket, `#[cfg(unix)]` with
  a non-unix stub; pre-recorded clips play anywhere), announcement templates,
  FNV-hash clip cache (capped at 500 files), fixed-gain mixer overlay.
- `src/youtube/`: yt-dlp process management (feeder) + metadata.

## Logging

- Default `warn` for all crates. `RUST_LOG` preset (`trace|debug|info|warn|
  error`) raises this app's level while keeping deps quiet, or pass a raw
  `EnvFilter`. Audio diagnostics on the `audio_stream` target at `debug` (5s).
