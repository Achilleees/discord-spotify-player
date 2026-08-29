# PORT.md — transplanting spotibot v0.5 into nob (Phase 1c)

This repo is the hardened reference implementation of the music stack. Its job
is to be transplanted into `never-off-beat` (nob) as Phase 1c, then retired.
This file is the transfer dossier: what maps where, what was paid for in blood,
what NOT to bring across.

## The 12 decisions (locked 2026-07-10)

1. **Strategy** — harden spotibot fully first, prove it live, then port.
2. **YouTube branch** — merged into v0.5 (yt-dlp/ffmpeg, mixed queue, DJ TTS).
3. **Quality bar** — full audit burn-down (2 audits, 8 lenses each).
4. **Authorization** — nob's rule: sharing the bot's voice channel is required
   to control playback (buttons, `/queue`, `/play`, `/skip`, `/stop`, and
   taking over a session via `/login`). Deliberate exceptions: when the bot is
   not yet in voice, `/play` accepts a requester in any voice channel and the
   bot joins them (fresh-boot path); `/announce` is a guild-level toggle, not
   playback control, and is ungated so it can be set before the bot joins.
5. **OAuth** — device authorization grant (RFC 8628) on Spotify's desktop
   client id (no app of our own, no client secret). Since 2026-08-10 Spotify
   rejects playback for tokens minted by third-party client IDs
   (librespot#1737); the desktop client id is what Spotify's own clients use,
   so tokens from it play. `/login` requests a device code, replies with a
   link to spotify.com/pair and the short code, then polls
   `accounts.spotify.com` inline inside the deferred interaction (10-min cap,
   `DEVICE_LOGIN_MAX_WAIT`). The poll is cancellable by a newer `/login`,
   `/logout`, or `/forget` via a per-user `Notify` (`Handler.pending_auth`).
   After the poll succeeds, the take-over rule (must share the bot's voice
   channel to evict the active DJ) is re-checked; on failure the tokens are
   stored inactive and the user re-runs `/login` to activate.
6. **Token storage** — SQLite `spotify_credentials` table, tokens in an
   encrypted `auth_blob` (XChaCha20-Poly1305; key = PBKDF2-HMAC-SHA256 of
   `TOKEN_ENC_KEY`, 600,000 iterations, fixed app salt
   `discord-spotify-player:token-enc:v1` — see `users/crypto.rs`).
7. **Discovery (mDNS)** — deleted. OAuth-only, like nob.
8. **Token refresh** — proactive (single-owner task, `expires_in` − 5 min) plus
   an early-refresh Notify signal from the librespot task on session death.
9. **Tests** — nob-style, portable (101 unit tests).
10. **Docs** — full rewrite + this file.
11. **Songbird/DAVE** — songbird 0.6 stable (same as nob), not the fork.
12. **Kickoff** — plan → merge → burn down by slice, each slice deployable.
13. **Web API dropped** — the shared desktop client ID is rate-limited on
    `api.spotify.com`, so the bot makes no calls there at all. Playback
    control (prev/next/queue/buttons) goes straight to the live `Spirc` via
    `SpircCommand`; track metadata (title/artist/album art) comes from
    librespot's own `PlayerEvent::TrackChanged` instead of a Web API fetch.
    This needed `Spirc::add_to_queue`, which isn't in a librespot release yet
    — `Cargo.toml` pins the four librespot crates to the git `dev` branch
    (rev `1599145`, 2026-08-22) instead of crates.io `0.8`. Bump to the next
    crates.io release (0.9+) once `add_to_queue` and the device-auth fixes
    ship there; drop the git pin at the same time.

## Module map: spotibot → nob-music

nob-music's internal modules are laid out in nob's `ARCHITECTURE.md`. The mapping:

| spotibot file | nob-music target | Notes |
|---|---|---|
| `src/oauth/mod.rs` | `spotify` (OAuth client) | device flow: `request_device_code` / `poll_device_token` / `refresh` — port as-is. |
| `src/users/mod.rs` + `crypto.rs` | `spotify` (credential store) + nob-core db | Table schema already matches nob's `spotify_credentials`. See "storage" below. |
| `src/spotify/player.rs` | `spotify` (session lifecycle) | `run_with_token` = the Spirc lifecycle. Drop `SpotifyPlayer` struct-of-statics for a module. Also owns playback control: a `SpircCommand` channel (`Play`/`Pause`/`Next`/`Previous`/`AddToQueue`) is applied to the live `Spirc` — port this channel, not a Web API client. |
| `src/spotify/sink.rs` | nob-audio (DSP) + `spotify` (sink) | Biquad/DSP belongs in nob-audio (already has DSP); the `Sink` impl stays in music. |
| `src/spotify/metadata.rs` | *(deleted)* | Was a raw reqwest Web API track fetch; removed — track metadata now comes from librespot's `PlayerEvent::TrackChanged` (see decision #13). Nothing to port. |
| `src/audio_bridge.rs` | nob-audio (ring buffer) | nob-audio already has a tested ring buffer — reconcile, keep nob's, port the even-frame parity guard if missing. |
| `src/audio/dj.rs` | `dj` | **Kokoro is a Unix socket here, not HTTP.** nob's TASKS say HTTP REST :8880 — reconcile (see gotcha). |
| `src/audio/mod.rs` (join sound) | `dj` or `actions` | Small; place with sound effects. |
| `src/discord/voice.rs` | `voice` | `SimpleBridgeReader` = the Songbird source adapter. |
| `src/presence.rs` | `presence` (or a shared-types module) | The shared `PresenceUpdate` enum (`Idle` / `Paused { title, artist, track_id }` / `Playing { title, artist, track_id, album_art_url }`, sourced from `PlayerEvent::TrackChanged` — no `access_token`) — both the player-event side and `src/discord/presence.rs` depend on it; give it an explicit home. |
| `src/discord/presence.rs` | `presence` | Bot status text. |
| `src/discord/bot.rs` | **split across `commands`/`actions`/`panel`/`player`** | **Do NOT port wholesale** — see below. |
| `src/queue.rs` | `queue` | Priority queue (DJ overlay > queue interrupts > Spotify baseline). Capped at `MAX_QUEUE_LEN = 500` (matches nob's unified-queue cap); `push()` is fallible (`-> bool`), rejecting at capacity. |
| `src/youtube/*` | `youtube` | yt-dlp feeder + metadata. Cookies/age-gate contract: `--cookies` is passed only when the file exists on disk (both metadata and feeder paths); after metadata succeeds there is NO `age_limit` reject (reaching metadata means cookies already unlocked the video); the no-cookie age-gate failure is classified from stderr into an actionable `AgeRestricted` error pointing the admin at `YOUTUBE_COOKIES`. |
| `src/config.rs` | nob-core config | Merge the Spotify/token keys into nob's config struct. Also capture the five module-local env reads that bypass `config.rs`: `YOUTUBE_COOKIES` (default `/var/lib/spotibot/youtube-cookies.txt`) and `YOUTUBE_TMP_DIR` (default `/tmp/spotibot-youtube`) in `youtube/mod.rs`, `DJ_CLIPS_DIR` / `DJ_CACHE_DIR` and `KOKORO_SOCKET` in `audio/dj.rs`. |
| `src/setup.rs` | (drop) | The CLI wizard is a spotibot-local convenience; nob is VPS-deployed. |

## What NOT to port

- **`bot.rs` as one file.** It is a ~2,500-line god-module because spotibot has
  no crate boundaries. nob's panel/actions/commands split supersedes it:
  the embed builders → `embeds`, the button/command dispatch → `commands` +
  `actions`, the presence loop → `presence`, the priority-queue manager →
  `player`/`queue`. Rebuild against nob's seams, don't paste.
- **The setup wizard** (`setup.rs`) — VPS deployment, not first-run CLI.
- **The `.env` in-place rewriter** — nob configures from env/secrets.env.
- **`SpotifyPlayer` as a struct with only associated fns** — make it a module.

## Paid-for gotchas (the expensive lessons)

- **DAVE / songbird.** Discord made the DAVE voice-encryption protocol
  mandatory (~March 2026). It broke playback on old songbird; spotibot ran a
  `beerpsi-forks/songbird` `davey` branch for months. songbird 0.6 stable has
  DAVE natively — use it (nob already does). Never pin a git *branch* for a
  voice dep; a `cargo update` silently re-resolves to the tip.
- **Spirc lifecycle.** `Spirc::new` can hang; wrap it in a 15s timeout (see
  `run_with_token`). Pin `spirc_task` inline (`tokio::pin!` inside a `select!`
  against the command receiver) so session death breaks out to the reconnect
  path and cancellation propagates; guard the event loop with `AbortOnDrop`.
  Do NOT detach-spawn the spirc task — that was the old design, and it parked
  forever on `cmd_rx.recv()` while the session died underneath it. After a
  session, `spirc.shutdown()` then drop.
- **Unrefreshable stored tokens must self-heal.** Some stored refresh tokens
  can never refresh (revoked, or minted before a client-id change — the
  v0.4→v0.5 live-VPS failure, and again the 2026-08-10 third-party-client-ID
  crackdown that forced the PKCE→device-flow switch). On auto-start refresh
  failure: deactivate the stored row and post a `/login` prompt to the text
  channel. On reactivation failure: deactivate and fall through to
  `/login → pair code` instead of dead-ending the user into `/forget` +
  `/login`. Never leave a dead row active and silently retry it every boot.
- **Never clear the bridge on `EndOfTrack`.** It is a natural track boundary —
  clearing there trims the tail of every auto-advancing track. Real stops are
  handled by `PlayerEvent::Stopped`; a priority-item drain clears the bridge
  itself before playing. (Paid for in commit 985db75.)
- **Queue drains are single-owner.** Serialize drains with an `AtomicBool`
  `compare_exchange` plus an abort-safe guard (`DrainGuard`) that clears the
  flag on drop, so a cancelled or panicking drain can't wedge all future
  drains. This covers the /play-triggered-drain vs end-of-track-manager race.
  (Paid for in commit 3aa49ef.)
- **Token refresh is single-owner.** Two things refreshing the same refresh
  token race and rotate it out from under each other. The fix: one refresher
  task owns it; everything else reads the current access token from shared
  state and *signals* the refresher (Notify) rather than refreshing itself.
- **The OAuth access token goes stale independently of the librespot session.**
  A healthy >1h Spirc session keeps streaming, but the captured access token
  still expires, and a reconnect (fast-reconnect loop or session death) needs
  a live one to re-authenticate `Spirc::new`. Proactive refresh + reading the
  token fresh from `ActiveSession` at call time both matter — even though
  buttons/queue/metadata no longer make Web API calls (decision #13), the
  token itself is still load-bearing for reconnects.
- **Device flow needs no callback.** The old Authorization Code + PKCE flow
  redirected to `127.0.0.1` on the *user's* machine, where nothing listens on
  a headless box — hence the old paste-back UX. The device authorization
  grant (RFC 8628) sidesteps that entirely: the bot requests a code, the user
  enters it at spotify.com/pair on any device, and the bot polls for the
  token. No listener, no redirect URI, no URL parsing, works the same on a VPS
  as on a desktop.
- **Pacing lives in the sink, not the reader.** `DiscordSink::write` sleeps/spins
  to real time; the reader's 10 ms sleep only fires on starvation. Don't move
  pacing to the reader.
- **Stereo frame alignment.** The ring buffer must drop/drain on even (L/R)
  boundaries or the channels swap permanently. Tested in `audio_bridge`.
- **Kokoro transport mismatch.** spotibot's `dj.rs` talks to Kokoro over a Unix
  domain socket (`/var/lib/spotibot/kokoro.sock`), `#[cfg(unix)]`-gated (all
  Unix-family targets, with a non-unix stub). nob's
  design doc says HTTP REST on `:8880`. Pick one at port time and update nob's
  TASKS — they currently disagree.

## Storage detail

`spotify_credentials` schema (matches nob's `002-music.sql`):
`discord_user_id TEXT PK, discord_name TEXT, spotify_username TEXT, auth_blob BLOB, is_active INTEGER, last_used_at TEXT, created_at TEXT`.

The `auth_blob` is a versioned, self-describing blob: byte 0 = scheme —
`0x00` plaintext, `0x02` XChaCha20-Poly1305 with AAD (`V_XCHACHA_AAD`), then
`[nonce(24) || ciphertext]`. The ciphertext is AAD-bound to the row owner
(`aad = discord_user_id`), so a blob copied onto another user's row won't
open. Downgrade protection: when a key is configured, plaintext (`0x00`) rows
are rejected, not silently accepted. There is no `0x01` handler. Key rotation
is a re-seal, not a migration. Port `users/crypto.rs` verbatim.

In nob, wrap DB access in nob-core's async `Db::call` instead of spotibot's
synchronous `Mutex<Connection>` (spotibot stayed sync to bound the diff; the
audit's atomicity/corruption concerns are already handled by SQLite upserts).

## nob TASKS to amend at port time

- Phase 1c credential storage: change "SQLite, encrypted" to name the scheme
  (XChaCha20-Poly1305, `TOKEN_ENC_KEY`).
- OAuth: record the device authorization grant (RFC 8628) decision and the
  desktop-client-id rationale.
- Refresh: record the single-owner proactive + Notify architecture.
- Amend "Spotibot is reference only; everything from scratch" → "hardened
  spotibot modules transplant with adaptation" (the from-scratch doctrine
  predates a proven implementation).
- DJ: reconcile Kokoro transport (socket vs HTTP).
