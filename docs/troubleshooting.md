# Troubleshooting

## Bot doesn't show up in Spotify's device list

**Cause:** no Spotify session is running for any user, or the session hasn't
finished its handshake with Spotify's servers yet.

**Fix:** run `/login` and complete the pairing at the link it gives you. If a
session was previously running, check the logs around startup for
`auto-start token refresh failed` — a stored refresh token that's gone stale
gets deactivated automatically and needs a fresh `/login`. `/who` tells you
whether any session is currently marked active.

## "Spotify is in use on another device" / takeover prompt

**Cause:** Spotify's active-device slot is claimed by something else (the
DJ's phone, another Spotify client), and the bot's device is only
*connected*, not *active*. The bot never claims the active slot on its own —
see `docs/architecture.md`'s device-activation section — it only claims it
on `/login` or a human pressing ▶.

**Fix:** press ▶ on the now-playing card (or `/login` again if you're
reconnecting) to explicitly take over the device.

## yt-dlp fails / 403s on YouTube links

**Cause:** YouTube changes its extraction contract often enough that an
outdated yt-dlp starts failing. Age-restricted or region-locked videos also
need cookies.

**Fix:**
- Update yt-dlp to the latest release — this is the most common fix for a
  sudden wave of failures.
- For age-restricted videos, point `YOUTUBE_COOKIES` at a cookies.txt file
  (Netscape format) exported from a logged-in browser session.
- A generic failure (network error, bad URL, extraction failure) surfaces to
  the user as *"couldn't fetch that link — check the URL and try again"* —
  check the bot's logs for the actual yt-dlp stderr, which is logged at
  `warn` (`yt-dlp failed`).

## Nothing audible, but the card shows "playing"

**Cause:** the shared `AudioBridge` is starved — a producer (librespot, the
YouTube/file feeder) isn't pushing samples fast enough, or `PREBUFFER_SECONDS`
is too low for the network conditions and Songbird is reading past the
filled portion.

**Fix:**
- Check the `audio_stream` log target at `debug`
  (`RUST_LOG=debug`, or `RUST_LOG="warn,audio_stream=debug"`): it logs
  `buf_len`, cumulative pushed/pulled/dropped counts, and the last push/pull
  timestamps every 5 seconds. A `buf_len` sitting near zero with pulls still
  happening means the buffer is starved.
- Raise `PREBUFFER_SECONDS` (0.0–8.0) or `AUDIO_BUFFER_SECONDS` (1–12) if the
  network to Spotify/YouTube is slow or bursty.
- Confirm the expected producer is actually feeding: a Spotify track should
  be pushing from `DiscordSink`; a queue item from the YouTube/file feeder.
  If `bridge.spotify_muted()` is stuck true while Spotify should hold the
  turn, that points at a turn-gate bug rather than a starved buffer — check
  the `player` target for the actor's last `active`/`sp` state.

## `/skip`, `/stop`, or ⏯ report "Nothing is playing right now"

**Cause:** this is the pure core's own answer when there's genuinely nothing
to act on — an empty queue and an idle or inactive Spotify device.

**Fix:** not a bug by itself. If it's unexpected, check `/np` and the
`player` target's last `step` log line for what `active`/`sp`/`queue_len`
actually were at that moment — a stale expectation (e.g. a track you think
is still queued but that already played) is the usual cause.

## 429 from `api.spotify.com`

**Cause:** this must never happen — the bot makes no calls to
`api.spotify.com` at all (device auth uses `accounts.spotify.com`; playback
control and metadata go straight over the live `Spirc`/librespot session).
A 429 in the logs from `api.spotify.com` is a regression, not an operational
issue: something re-introduced a Web API call.

**Fix:** grep the codebase for `api.spotify.com` and `reqwest` calls outside
`src/oauth/mod.rs` — anything found there needs to be routed through Spirc
(`src/spotify/player.rs`) instead, per the design in `docs/PORT.md`.

## Reading the logs

- `RUST_LOG=debug` gives full detail on the app's own crates (see
  `docs/configuration.md` for the preset/raw-filter distinction) while
  keeping `serenity`/`songbird`/`librespot` at `warn`.
- The `player` target shows **every** `Input` the actor receives and the
  resulting `active`/`sp`/`armed`/`device_active`/`queue_len`/`effects` after
  each `step` — this is the first place to look for "what did the player
  actor decide, and why" for any playback-ordering question.
- The `audio_stream` target shows buffer health every 5 seconds — the first
  place to look for audio dropouts or silence.
- Startup logs (`info` and above) show intent/session lifecycle: OAuth
  session start, auto-start attempts, yt-dlp/ffmpeg availability checks.
