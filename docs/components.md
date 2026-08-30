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
  Only the turn holder (the player actor) clears it — the Spotify layer never
  does.
- **DiscordSink** (`src/spotify/sink.rs`): librespot's audio backend. Applies
  optional DSP (preamp + biquad low/high shelves) and paces output to real time,
  then pushes into the bridge. Hot path — no allocations or heavy logging.
- **SimpleBridgeReader** (`src/discord/voice.rs`): `Read + Seek + MediaSource`
  for Songbird. Prebuffers on first read (honors `PREBUFFER_SECONDS`), then
  pulls from the bridge.
- **Priority model** (`src/queue.rs`, owned by the player actor in
  `src/player/`): DJ overlay > the queue (Spotify tracks and
  YouTube/SoundCloud/files, in true order — `MediaSource::Spotify | YouTube |
  File`, YouTube covers SoundCloud via yt-dlp) > Spotify Connect baseline.
  Radio rules: tracks play strictly in queue order regardless of source, and
  the bot never skips a track on its own — a Spotify `Next` is sent only on a
  human ⏭/`/skip`. The player actor's `PlayerState` holds the queue, the
  armed Spotify track, and the "turn" (who is entitled to be audible); its
  pure core arms the first Spotify track anywhere in the queue into
  Spotify's own queue (`SpircCommand::AddToQueue`) while Spotify holds or is
  about to hold the turn, so librespot's own track-end advance lands on it —
  any media items ahead of it play first, then Spotify resumes onto the
  armed track once it's confirmed via `Playing`.

## Player core (`src/player/`)

- **`state.rs`** — the pure decision core: one owned `PlayerState` (the
  queue, the armed Spotify track, the turn, a mirror of Spotify's own
  transport state, pause provenance, an inflight-command ring for
  pause-echo detection), advanced by `step(state, input, now) -> Vec<Effect>`.
  Imports no serenity, songbird or librespot-connect types — only `std`, a
  plain `tokio::sync::oneshot` handle, `librespot_core::SpotifyUri` and
  `crate::queue` — so every behaviour is deterministic under test. This is
  the piece that ports to nob unchanged (74 tests).
- **`actor.rs`** — the impure shell around the core: one task owns the
  `PlayerState`, the bridge-reader `TrackHandle`, the feeder cancel/pause
  flags, and the current Spirc command sender. Reached only through
  `PlayerHandle` (`enqueue`/`skip`/`stop`/`toggle_pause`/`previous`/`query`/
  `lookup_spotify`, plus a raw `send` for fire-and-forget inputs like
  `Transport`/`LinkUp`/`Tick`). **The actor awaits nothing, ever**: every
  `Effect` is a synchronous channel send, an atomic store, or a
  `tokio::spawn` (media runners, voice joins, DJ announcements, timers).
  `ensure_session` and Spotify metadata lookups run in the caller's task
  (the interaction handler) *before* an `Enqueue` is sent — never inside the
  actor, so a step can never park the mailbox behind a link it's waiting on.
- Effects reach three other tasks: the UI task (`discord/ui.rs`,
  `Effect::Ui`), the presence loop (`discord/presence.rs`,
  `Effect::Presence`), and the live Spirc (`Effect::Spirc`, via the sender
  the session supervisor publishes on switch/stop).

## Spotify path (librespot + OAuth)

- OAuth-only (device authorization grant, RFC 8628); mDNS discovery was
  removed in v0.5.
- `src/oauth/mod.rs`: `request_device_code`, `poll_device_token`, token
  refresh — all against Spotify's desktop client id. No profile fetch; the
  Spotify session's display name is the Discord display name.
- `src/spotify/session.rs`: `SessionSupervisor` owns the Spotify session's
  own lifecycle — the librespot task, its proactive token refresher, the
  shared token state, and a monotonic session generation. Background:
  started by `/login`, boot auto-start, or on demand via `ensure_session`
  when a Spotify link is queued with the link down. **Imports no songbird,
  queue or player-effect type** — its only surface into the player is
  `player::state::Input`, delivered through the same `PlayerHandle` mailbox
  as every other source, so an account switch mid-track structurally cannot
  reach the queue, the feeder or a `TrackHandle`. `link_up_watch()` exposes
  the current generation as a `watch::Receiver`, written without the
  supervisor's `switch`/`stop` lock, so `ensure_session` can wait on it while
  the device-auth pairing poll (which can run minutes) holds no lock of its
  own.
- `src/spotify/player.rs`: `run_with_token` drives the Spirc session
  lifecycle (15s `Spirc::new` timeout, reconnect loop, event →
  `TransportEvent`). It also owns playback control: `SpircCommand`
  (`Play`/`Pause`/`Next`/`Previous`/`AddToQueue`/`Load`/`Lookup`/
  `ActivateDevice`/`Transfer`) arrives over a channel and is applied to the
  live `Spirc` — no calls to api.spotify.com. It no longer clears the shared
  audio bridge; only the player actor (the turn holder) does.
- Now-playing track metadata comes from librespot itself:
  `PlayerEvent::TrackChanged` carries the `AudioItem` (title, artist,
  track_id, album art), forwarded to the player actor as a `TransportEvent`.
  Metadata for a Spotify item *queued* ahead of time comes from
  `SpircCommand::Lookup` (`Track::get(&session, &uri)` on the live session)
  instead — still no Web API call.

## Discord path (serenity + songbird)

- `src/discord/bot.rs`: gateway handler — slash commands (`/login`,
  `/logout`, `/forget`, `/who`, `/play`, `/queue`, `/skip`, `/stop`, `/np`,
  `/announce`), button interactions, the device-authorization `/login` flow,
  account switching (`switch_active_session` → `SessionSupervisor::switch`),
  and the voice-join + auto-leave logic. Talks to playback only through a
  `PlayerHandle`.
- `src/discord/ui.rs`: single owner of the now-playing/controls card, keyed
  on one `card_id`. Both the Spotify baseline and the queue post through its
  mailbox (`UiMsg`) rather than touching the channel directly, so the two
  playback sources can never race each other's post/delete. Owns every
  embed builder.
- `src/discord/voice.rs`: bridge reader + Songbird track events.
- `src/discord/presence.rs`: bot status text + the presence loop
  (`run_presence_loop`), fed by `Effect::Presence`.
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
