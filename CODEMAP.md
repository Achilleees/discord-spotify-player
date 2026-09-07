# CODEMAP

Technical file map. For behavior and rationale, see `docs/architecture.md`,
`docs/commands.md`, and `docs/PORT.md`.

## Workspace

`Cargo.toml` defines the default `discord-spotify-player` member and the thin
`crates/nob` host. Root build/run commands preserve the existing Spotibot
binary; `cargo build --workspace --release --locked` builds both executables.
CI checks all members. `tests/host_cli.rs` and `crates/nob/tests/cli.rs` exercise
offline configuration through both real entry points using synthetic fixtures.
Both hosts inherit their version from `[workspace.package]` in `Cargo.toml`;
the release policy and upcoming version are recorded in `CHANGELOG.md`.

## `src/`

```
src/
├── main.rs              executable entry point: Tokio runtime, calls the
│                         library's discord_spotify_player::run()
├── lib.rs               library entry point and private module tree:
│                         logging, config load, YouTube/ffmpeg
│                         checks, OAuth client, credential store, AudioBridge,
│                         Discord bot startup, then parks
├── runtime.rs            profile/CLI, isolated env maps, resolved paths and
│                         process-held state locks; no playback policy
├── config.rs             .env → Config: required Discord ids, audio tuning,
│                         TOKEN_ENC_KEY; numeric parsing with warn-on-typo,
│                         strict soundboard volume percent → clip gain
├── setup.rs               `--setup` first-run wizard: prompts for the bot
│                         token, picks guild/channel via the Discord API,
│                         writes `.env`
├── presence.rs            PresenceUpdate enum (Idle/Paused/Playing) — the
│                         player actor's output vocabulary for bot status
├── audio_bridge.rs         AudioBridge: the shared VecDeque<f32> ring buffer
│                         (44.1kHz stereo) every audio producer pushes into
│                         and Songbird pulls from; turn-gated music clear,
│                         bounded owned overlay lane, ducking and pause gate
├── queue.rs                MediaSource (Spotify/YouTube/File) + QueueItem +
│                         PriorityQueue: the one ordered queue across sources
│
├── history/
│   └── mod.rs             HistoryStore: append-only log of what aired,
│                          written when a track becomes audible (never when
│                          it is queued); keeps the context uri each track
│                          played from, and walks backwards by row id for ⏮
├── queue_store.rs          QueueStore: the pending queue on disk, rewritten
│                          whole on every change; its own wire format, so
│                          queue.rs stays free of serde
│
├── routing/
│   ├── mod.rs             paired config, typed requests/replies, room/activity selection
│   └── transport.rs        authenticated loopback frames, bounded request journal
│
├── soundboard/
│   └── mod.rs             startup local catalogue, path/size validation and
│                          bounded ffmpeg decoding; no Discord or music state
│
├── player/
│   ├── mod.rs             module doc: how state.rs and actor.rs divide work
│   ├── state.rs            pure decision core: PlayerState, Active (the
│   │                       turn), step(state, input, now) -> Vec<Effect>;
│   │                       imports no serenity/songbird/librespot-connect
│   └── actor.rs             impure shell: one task + mailbox owning
│                            PlayerState; PlayerHandle is the typed API
│                            (enqueue/play/skip/stop/stop_without_leaving/
│                            toggle_pause/previous/clear_queue/restore_queue/
│                            query/lookup_spotify); runs media runners as spawns
│                            and rechecks the Back caller's voice guard after
│                            its request-correlated history read completes
│
├── spotify/
│   ├── mod.rs              re-exports (SpircCommand, EnsureOutcome, SessionSupervisor)
│   ├── session.rs           SessionSupervisor: the Spotify session's own
│   │                       lifecycle (librespot task, token refresher,
│   │                       generation); imports no songbird/queue/player-effect
│   │                       type — only Input reaches the player
│   ├── player.rs             run_with_token: drives the live Spirc session,
│   │                       applies SpircCmd, translates PlayerEvent into
│   │                       TransportEvent — no api.spotify.com calls
│   └── sink.rs               DiscordSink: librespot's audio backend; DSP
│                            (preamp/bass/treble) + real-time pacing; turn
│                            gate via bridge.spotify_muted()
│
├── oauth/
│   └── mod.rs               SpotifyOAuth: device authorization grant (RFC
│                            8628) against Spotify's desktop client id —
│                            request_device_code, poll_device_token, refresh
│
├── users/
│   ├── mod.rs                UserStore: SQLite spotify_credentials +
│   │                        settings tables; per-user CRUD, exclusive-active
│   └── crypto.rs              TokenCipher: XChaCha20-Poly1305 blob
│                             encryption, PBKDF2 key stretching from
│                             TOKEN_ENC_KEY, owner-bound AAD
│
├── audio/
│   ├── mod.rs                join-sound generation
│   └── dj.rs                  DJAnnouncer: Kokoro TTS client (Unix socket)
│                             + pre-recorded clips, template selection,
│                             FNV-hash cache; actor submits to shared overlay lane
│
├── youtube/
│   ├── mod.rs                yt-dlp/ffmpeg availability checks, tmp-dir/
│   │                        cookies path resolution
│   ├── metadata.rs            fetch_youtube_metadata via `yt-dlp --dump-json`;
│   │                        bounded YouTube text-search results, duration
│   │                        cap, live-stream rejection, attachment validation
│   ├── probe.rs               shared metadata-only subprocess: concurrency
│   │                        cap, timeout, kill-on-drop and bounded output
│   └── feeder.rs               feed_youtube_to_bridge / feed_file_to_bridge:
│                              spawn yt-dlp|ffmpeg, decode, push PCM into
│                              AudioBridge, cancellable
│
└── discord/
    ├── mod.rs                 re-exports DiscordBot
    ├── bot.rs                  Handler struct + EventHandler impl: ready(),
    │                          voice_state_update (empty-channel teardown,
    │                          own-disconnect teardown), DiscordBot::new/
    │                          start_background
    ├── commands.rs              slash-command registration + dispatch:
    │                          voice-gate checks, /play /queue /skip /stop
    │                          /np /announce, button routing, shared track
    │                          resolution/enqueue with post-lookup voice check
    ├── front.rs                 nob's compact slash surface, private performer menus
    ├── routing.rs               performer execution, guarded actions, local OAuth pairings
    ├── voice_owner.rs           room leases, revision fences, serialized voice transitions
    │                          music priority over visits, same-room overlay leases
    ├── soundboard.rs            nob-only private clip picker: pagination, expiry,
    │                          single-use owner/guild/room/revision-bound menus
    ├── visits.rs                idle clip tracks / same-room bridge overlays,
    │                          audience checks, cancellation, completion/deadlines
    │                          and fenced cleanup; only idle visits leave voice
    ├── search.rs                Add music modal, private result rendering,
    │                          bounded owner/guild/expiry-checked single-use menus
    ├── admin.rs                  nob-only slowmode/purge: invocation permissions,
    │                          bounded cleanup, pin/bot-message preservation
    ├── account.rs                /login /logout /forget, device-code poll,
    │                          account-switch bookkeeping, boot auto-start
    ├── ui.rs                     one task owning the now-playing/controls
    │                          card (UiMsg mailbox, one card_id), cached pause/
    │                          account state and periodic missing-card recovery
    ├── voice.rs                  SimpleBridgeReader (Songbird MediaSource,
    │                          prebuffers on first read), TrackErrorHandler
    └── presence.rs                run_presence_loop: renders PresenceUpdate
                                as the bot's Discord activity line
```

## Data flow

- **Discord → player.** `discord::bot`/`commands`/`account` translate slash
  commands and button interactions into `player::state::Input`s, sent
  through `PlayerHandle`'s mailbox. The gateway event handler never touches
  playback state directly.
- **Spotify session → player.** `spotify::session::SessionSupervisor` runs
  the librespot task in the background and forwards its lifecycle
  (`LinkUp`/`LinkDown`/`LinkReconnecting`, each generation-tagged) as
  `Input` through the same mailbox. `spotify::player::run_with_token`
  emits decoded transport telemetry (`Playing`/`Paused`/`EndOfTrack`/
  `TrackChanged`/`SetQueue`/…) as `TransportEvent`s on a channel;
  `discord::bot::transport_shim` wraps them as `Input::Transport { gen, ev }`
  into the same mailbox.
- **Player → everything else.** `step()` returns `Effect`s; the actor
  (`player::actor`) executes them: `Effect::Spirc` onto the live Spirc
  session, `Effect::StartMedia`/`CancelMedia` as spawned media runners
  (`youtube::feeder`) feeding `audio_bridge::AudioBridge`, `Effect::Ui` onto
  `discord::ui`'s mailbox, `Effect::Presence` onto
  `discord::presence::run_presence_loop`, `Effect::Announce` onto
  `audio::dj::DJAnnouncer`.
- **Audio producers → Discord.** `spotify::sink::DiscordSink` (Spotify) and
  `youtube::feeder` (YouTube/SoundCloud/files) both push PCM into the one
  `AudioBridge`; `discord::voice::SimpleBridgeReader` pulls from it as
  Songbird's audio source. Only the player actor clears the bridge.
- **Soundboard → audio.** `discord::soundboard` validates private selections;
  `discord::visits` reserves a `VoiceOwner` lease and asks
  `soundboard::Catalogue` to decode one local clip. An idle visit uses a
  separate Songbird track, and music may preempt its temporary connection.
  In an existing music room, an overlay lease preserves the music connection
  and feeds `AudioBridge`'s bounded clip lane, shared with existing DJ speech.
  Music ducks temporarily; pause, seek and ordinary track transitions preserve
  the clip. Both paths use `Config.soundboard_volume`, configured with
  `SOUNDBOARD_VOLUME_PERCENT` (0–100, default 100). Cleanup stops only the owned
  clip, and only a still-owned idle visit may leave voice. Neither path inserts
  a queue item or receives voice audio.
- **Credentials.** `discord::account` reads/writes through `users::UserStore`,
  which encrypts token blobs via `users::crypto::TokenCipher`. Neither the
  player nor the Spotify session touches SQLite directly.

## Tests

Most tests are inline `#[cfg(test)] mod tests` in the file they cover;
the workspace also has the host CLI tests listed above.
`cargo test --workspace --locked` runs both. The pure core carries most
of the suite:

- `src/player/state.rs` — the bulk of the crate's tests: `step()` behavior
  per input/state combination (radio-order rules, arming/ack, pause
  provenance, timers, snapshots), deterministic since the core takes no IO.
- `src/discord/commands.rs` — voice-gate policy, Spotify link/track-id
  parsing, now-playing rendering.
- `src/soundboard/mod.rs` — catalogue schema, local path and size limits,
  bounded decoding and malformed/oversized audio rejection.
- `src/discord/soundboard.rs`, `src/discord/voice_owner.rs`,
  `src/discord/visits.rs` — private menu/page bounds, requester and revision
  checks, visit/music priority, overlay ownership, stale cleanup and departure
  bookkeeping.
- `src/config.rs` — env parsing (snowflake validation, numeric clamping,
  text-channel fallback, strict finite soundboard volume bounds).
- `src/users/mod.rs`, `src/users/crypto.rs` — credential store CRUD,
  encryption roundtrip, plaintext-downgrade rejection, wrong-key handling.
- `src/youtube/metadata.rs` — yt-dlp JSON mapping, duration cap, live-stream/
  age-limit handling.
- `src/audio_bridge.rs`, `src/discord/voice.rs` — buffer push/pull framing,
  prebuffer timing, bounded complete overlays, ducking, pause and cancellation.
- `src/queue_store.rs` — round-trip persistence, replace-not-append, one
  unreadable row skipped rather than losing the rest.
- `src/history/mod.rs` — newest-first ordering, context retention, and the
  id-walk semantics back-navigation depends on.
- `src/oauth/mod.rs`, `src/discord/presence.rs`, `src/audio/mod.rs`,
  `src/spotify/sink.rs` — smaller focused suites per module.
