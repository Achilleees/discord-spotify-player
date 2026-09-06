# PORT.md — shared Spotibot and nob continuity

## Current direction (accepted 2026-09-06)

Continue this repository and Spotibot's Git history as the foundation for
both bots. Evolve the existing package into a Cargo workspace and adapt nob's
useful menus and server modules around the hardened playback implementation.
The eventual project name is `never-off-beat`.

- **Two Discord identities, two processes, shared source.** Spotibot remains
  the music specialist; nob adds server utilities, soundboard and companion
  features. Music features overlap through the same implementation.
- **Keep the existing owners.** The player actor/pure core owns playback,
  the session supervisor owns Spotify, and one UI task owns the card. Import
  nob's presentation and actions through these boundaries.
- **Isolate runtime state.** Each process needs its own configuration,
  database, caches and Spotify device identity. Existing queue and active-user
  tables are not safe for concurrent bot instances. Shared catalogues can be
  designed explicitly later.
- **Preserve behavior during extraction.** Retain current queue ordering,
  Spotify lifecycle, audio framing/pacing, setup and credential storage. Add
  menus and new playback capabilities incrementally with the relevant tests.
- **Continue the dev/CI workflow.** `dev` is the integration branch; `main`
  remains deployment-only. Local extraction does not require a prior live
  cutover. Deployment still requires green checks and explicit intent.
- **Retire the duplicate repository after migration.** Preserve nob's history
  and pending work, import the required features, then coordinate renaming
  this project and archiving the old nob repository under a legacy name.
  Both bot identities survive.

The default workspace member is `discord-spotify-player`; `crates/nob` adds
a thin, independently runnable host. Both call the shared library in
`src/lib.rs`. Music includes private search/link entry and richer playback
controls; nob additionally has slowmode, message cleanup and a private
soundboard. Configuration, databases, caches and generated Spotify device
identities are isolated; see
[two-bots.md](two-bots.md). Remaining server modules, companion
behavior and live two-room acceptance are tracked on the existing board.

Opt-in paired routing now makes nob the slash frontend (`/play`, `/music`,
`/soundboard`, `/server`) while Spotibot retains its own card buttons. Each
performer still owns its audio and account lifecycle; typed authenticated local requests
link behavior without sharing state. The [soundboard](soundboard.md) uses
that voice owner for bounded local-clip visits, with music taking priority.
Its private menu is available in standalone mode too. Local implementation
does not complete live acceptance or authorize deployment; random visits and
voice reception remain outside this feature.

## Historical transfer dossier — superseded migration plan

The sections below record the earlier one-way transplant into the old nob
repository. Their freeze/retirement instructions, destination module map,
storage rewrite and wizard-removal recommendations are historical, not the
current implementation plan. Keep the technical lessons as context; check
current source and the accepted direction above before applying them.

## Earlier decisions (2026-07-10 onward)

1. **Strategy** — harden spotibot fully first, prove it live, then port.
2. **YouTube branch** — merged into v0.5 (yt-dlp/ffmpeg, mixed queue, DJ TTS).
3. **Quality bar** — full audit burn-down (2 audits, 8 lenses each).
4. **Authorization** — nob's rule: sharing the bot's voice channel is required
   to control playback (buttons, `/play`, `/queue`, `/skip`, `/stop`, and
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
9. **Tests** — nob-style, portable (186 unit tests).
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
14. **Unified `/play` + `/queue`; priority items wait for track end** — one
    verb per intent: `/play` starts playback if nothing is playing and
    otherwise enqueues (`next:true` jumps the queue); `/queue` always
    enqueues and never starts playback. Both accept Spotify, YouTube,
    SoundCloud, and file attachments. A queued item interrupting a track
    mid-playback surprised users, so the priority-queue manager now waits
    for `EndOfTrack` before draining a queued item, and resumes Spotify
    afterwards only if it was playing before (`SpotifyState`: `Idle` /
    `Playing` / `Paused`, fed by `PresenceUpdate`). — *superseded: the
    priority-queue manager, `SpotifyState` and the wait-for-`EndOfTrack`
    drain are gone; the player actor's pure core now owns this decision —
    see decision 16.*
15. **Unified queue — radio rules; the bot never sends Next on its own** —
    decision #14 still routed Spotify links straight into Spotify Connect's
    own queue (`add_to_queue`), invisible to the bot: two queues, `/queue`
    showing half the truth, and ⏭ behaving inconsistently depending on which
    queue a "next" item actually lived in. A first fix tried a `Next`-based
    handoff (skip Spotify onto a bot-armed track), but that skipped playlist
    tracks out from under the DJ whenever Spotify auto-advanced on its own.
    Now there is one queue (`MediaSource::Spotify` joins `YouTube`/`File` in
    `src/queue.rs`) and tracks play strictly in that order regardless of
    source, like a radio — the bot never sends `Next`; the only way past a
    track is a user pressing ⏭/`/skip`. While Spotify is playing, the bot
    arms the *first Spotify track anywhere in the queue* into Spotify
    Connect's own queue (`add_to_queue`); librespot's own track-end advance
    then lands on it, so any media items ahead of it in the bot's queue play
    first (Spotify sits paused on the armed track at 0:00) and Spotify
    resumes onto it once they're done. The armed track pops from the bot
    queue when the matching `Playing` event confirms Spotify actually
    started it (also catches the DJ skipping from their phone). `load` is
    used only when Spotify is idle, because `load` replaces Spotify's
    context and stops (or autoplays) — using it while a context is live
    would wipe the DJ's playlist/album position, and librespot's queue has
    no remove operation to undo a wrong `add_to_queue`. The decision logic
    is one pure function, `head_action(head, spotify_state, media_active,
    trigger) -> HeadAction`, unit-tested per row of the trigger × head-kind
    table (see the plan at commit time); the only mutating critical section
    is `try_arm_first_spotify`, which the presence loop also drives on every
    Spotify `Playing` event so a chain of queued Spotify tracks stays
    gap-free without a fresh `/queue` each time. — *superseded:
    `head_action`, `try_arm_first_spotify`, `armed_spotify` and the
    presence-loop-driven re-arm are gone; the radio rules stand, but the
    owner is now the player actor's pure core, not a handler-shared mutex —
    see decision 16.*
16. **Player-core architecture — one owned state, a pure `step`, three
    lifecycles** — decisions 14 and 15 kept growing the same weak spot:
    playback intent was *inferred* from Spotify's own transport telemetry
    (a `spotify_state` snapshot, a `resume_spotify_after_drain` bool
    sampled once at drain start) instead of *owned*, which produced a class
    of freeze/double-play/audio-cut bugs no additional `head_action` arm
    could close (reproduced live: phone-pause → skip → media → skip → the
    next Spotify track never played, because the bool said "wasn't
    playing"). The fix has three parts, and it — not any single file — is
    what nob inherits:
    - **Owned state.** One player actor (`src/player/actor.rs`) owns all
      playback state — the queue, the armed Spotify track, and the "turn"
      (who is entitled to be audible) — in a single `PlayerState`, reached
      only through a mailbox. The actor awaits nothing, ever: every effect
      is a synchronous channel send or a `tokio::spawn` (media runners,
      voice joins, announcements, timers), so a decision can never be
      interleaved with the IO that carries it out.
    - **A pure decision core.** `src/player/state.rs`:
      `step(state, input, now) -> Vec<Effect>`, importing no serenity,
      songbird or librespot-connect types. Deterministic under test (74
      tests) — this is the module that ports to nob's `player` crate
      nearest to verbatim; port its test table as the spec, the same way
      `head_action`'s table was meant to be ported before it was outgrown.
    - **Three independent lifecycles.** The player (lives for the process,
      owns voice/bridge/queue), the Spotify session (`SessionSupervisor` in
      `src/spotify/session.rs` — background, started by `/login`/boot
      auto-start, or on demand via `ensure_session`; structurally unable to
      touch playback because it imports neither songbird nor the queue),
      and the account (`/login` registers/switches an active user, nothing
      more). Session churn (a `/login` mid-track) and playback state are now
      different compiler-enforced blast radii, not just a convention.

    Radio rules (decision 15) are unchanged in substance — strict queue
    order across sources, the bot never sends Spotify a `Next` on its own —
    but are now a property of `step`'s test table, not a hope resting on
    every call site remembering to check a shared mutex correctly. Port the
    three-lifecycle split and the "actor awaits nothing" discipline as
    architecture, not just `state.rs`'s code.

## Module map: spotibot → nob-music

nob-music's internal modules are laid out in nob's `ARCHITECTURE.md`. The mapping:

| spotibot file | nob-music target | Notes |
|---|---|---|
| `src/oauth/mod.rs` | `spotify` (OAuth client) | device flow: `request_device_code` / `poll_device_token` / `refresh` — port as-is. |
| `src/users/mod.rs` + `crypto.rs` | `spotify` (credential store) + nob-core db | Table schema already matches nob's `spotify_credentials`. See "storage" below. |
| `src/player/state.rs` | **`player`** (decision core) | The pure core: one owned `PlayerState`, `step(state, input, now) -> Vec<Effect>`. No serenity/songbird/librespot-connect types — port near-verbatim, including its test table (74 tests); that table *is* the spec (see decision 16). |
| `src/player/actor.rs` | **`player`** (runtime shell) | The impure shell: one task owns `PlayerState` plus the bridge-reader `TrackHandle`, feeder cancel/pause flags and the live Spirc sender, reached only through a `PlayerHandle` mailbox. Port the discipline, not the transport specifics — the actor awaits nothing, ever; every effect is a synchronous send or a `tokio::spawn`. |
| `src/spotify/player.rs` | `spotify` (session lifecycle) | `run_with_token` = the Spirc lifecycle. Drop `SpotifyPlayer` struct-of-statics for a module. Also owns playback control: a `SpircCommand` channel (`Play`/`Pause`/`Next`/`Previous`/`AddToQueue`/`Load`/`Lookup`/`ActivateDevice`/`Transfer`) is applied to the live `Spirc` — port this channel, not a Web API client. `Lookup(uri, oneshot)` resolves queued-Spotify-item metadata via `Track::get(&session, &uri)`, returning `TrackLookup { title, artist, album_art_url }`. No longer clears the shared bridge on any event — only the player actor (the turn holder) does. |
| `src/spotify/session.rs` | `spotify` (session supervisor) | `SessionSupervisor` owns the librespot task, its proactive refresher, the shared token state, and a monotonic session generation — kept structurally unable to reach playback (imports no songbird, queue, or player-effect type; its only surface into `player` is `Input`, delivered through the same mailbox as every other source). `ensure_session` is the on-demand entry point, called from the command-handling task, never from inside the player actor. Port the import restriction as an enforced crate boundary, not a convention. |
| `src/spotify/sink.rs` | nob-audio (DSP) + `spotify` (sink) | Biquad/DSP belongs in nob-audio (already has DSP); the `Sink` impl stays in music. |
| `src/spotify/metadata.rs` | *(deleted)* | Was a raw reqwest Web API track fetch; removed — track metadata now comes from librespot's `PlayerEvent::TrackChanged` (see decision #13). Nothing to port. |
| `src/audio_bridge.rs` | nob-audio (ring buffer) | nob-audio already has a tested ring buffer — reconcile, keep nob's, port the even-frame parity guard if missing. Only the turn holder may clear it. |
| `src/audio/dj.rs` | `dj` | **Kokoro is a Unix socket here, not HTTP.** nob's TASKS say HTTP REST :8880 — reconcile (see gotcha). |
| `src/audio/mod.rs` (join sound) | `dj` or `actions` | Small; place with sound effects. |
| `src/discord/ui.rs` | `embeds`/`panel` | Single owner of the now-playing/controls card: one task, one mailbox (`UiMsg`), one `card_id`. Port the single-owner discipline — nob's panel/embeds split must still resolve to exactly one writer of the card, or the two-owner race this repo just fixed comes back. |
| `src/discord/voice.rs` | `voice` | `SimpleBridgeReader` = the Songbird source adapter. |
| `src/presence.rs` | `presence` (or a shared-types module) | The shared `PresenceUpdate` enum (`Idle` \| `Paused` \| `Playing { title, artist }`, produced by the player actor from librespot's `PlayerEvent::TrackChanged` — no `access_token`, no `track_id`) — both the player actor and `src/discord/presence.rs` depend on it; give it an explicit home. |
| `src/discord/presence.rs` | `presence` | Bot status text. |
| `src/discord/bot.rs` | **split across `commands`/`actions`/`panel`/`player`** | **Do NOT port wholesale** — see below. It's wiring, not decision logic: slash-command/button dispatch, the device-auth `/login` flow, and voice join/leave all call into a `PlayerHandle` (queue/skip/stop/pause) and a `SessionSupervisor` (switch/stop); the radio-rules decisions themselves — arm, pop, whose turn it is — live in `player/state.rs`, not here (see decision 16). |
| `src/queue.rs` | `queue` | One priority queue (DJ overlay > queue > Spotify baseline; `MediaSource::Spotify \| YouTube \| File`, all in true order — see decisions #15/#16). Tracks play strictly in queue order (radio rules); while Spotify holds the turn, the *first* Spotify track anywhere in the queue is armed into Spotify's own queue instead (see the `player/state.rs` row). Capped at `MAX_QUEUE_LEN = 500` (matches nob's unified-queue cap); `push()`/`push_front()` are fallible (`-> bool`), rejecting at capacity; `insert(idx, item)` clamps `idx` to `len` and backs `next:true` behind an armed head; `peek()`/`pop_if`/`remove_first` back the arm/pop-on-`Playing` logic. Each item carries a queue-stamped `item_id` (names one residency in the queue, not the track itself — a re-inserted popped item gets a fresh id). |
| `src/youtube/*` | `youtube` | yt-dlp feeder + metadata. Cookies/age-gate contract: `--cookies` is passed only when the file exists on disk (both metadata and feeder paths); after metadata succeeds there is NO `age_limit` reject (reaching metadata means cookies already unlocked the video); the no-cookie age-gate failure is classified from stderr into an actionable `AgeRestricted` error pointing the admin at `YOUTUBE_COOKIES`. |
| `src/config.rs` | nob-core config | Merge the Spotify/token keys into nob's config struct. Also capture the five module-local env reads that bypass `config.rs`: `YOUTUBE_COOKIES` (default `/var/lib/spotibot/youtube-cookies.txt`) and `YOUTUBE_TMP_DIR` (default `/tmp/spotibot-youtube`) in `youtube/mod.rs`, `DJ_CLIPS_DIR` / `DJ_CACHE_DIR` and `KOKORO_SOCKET` in `audio/dj.rs`. |
| `src/setup.rs` | (drop) | The CLI wizard is a spotibot-local convenience; nob is VPS-deployed. |

## What NOT to port

- **The `discord/` module split as-is.** `bot.rs` (~600 lines of wiring),
  `commands.rs`, `account.rs` and `ui.rs` are cut along spotibot's needs, not
  nob's. nob's panel/actions/commands split supersedes them: `ui.rs`'s embed
  builders → `embeds`, the button/command dispatch → `commands` + `actions`,
  the presence loop → `presence`, `player/state.rs` + `player/actor.rs` →
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
- **An armed track must never be `add_to_queue`'d twice.** librespot's queue
  has no remove operation, so a double-arm plays a track twice with no way
  to undo it. `try_arm_first_spotify` is the only writer of `armed_spotify`
  and treats checking-`None`-then-arming as one critical section (lock the
  mutex, check, send `AddToQueue`, set, unlock) — never arm outside it.
- **Armed state goes stale the moment Spotify's own queue is gone.** Spotify
  loses its pending `add_to_queue` entry on `Idle`, a fresh `spawn_session`,
  `/logout`, `/forget`, and `/stop` — clear `armed_spotify` in all five, or
  the bot will wait forever for a `Playing` event that's never coming and
  leave the (still-queued, un-armed) track stuck behind a phantom lock.
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
