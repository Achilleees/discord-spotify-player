# CODEMAP

Technical file map. For behavior and rationale, see `docs/architecture.md`,
`docs/commands.md`, and `docs/PORT.md`.

## `src/`

```
src/
├── main.rs              entry point: logging, config load, YouTube/ffmpeg
│                         checks, OAuth client, credential store, AudioBridge,
│                         Discord bot startup, then parks
├── config.rs             .env → Config: required Discord ids, audio tuning,
│                         TOKEN_ENC_KEY; numeric parsing with warn-on-typo
├── setup.rs               `--setup` first-run wizard: prompts for the bot
│                         token, picks guild/channel via the Discord API,
│                         writes `.env`
├── presence.rs            PresenceUpdate enum (Idle/Paused/Playing) — the
│                         player actor's output vocabulary for bot status
├── audio_bridge.rs         AudioBridge: the shared VecDeque<f32> ring buffer
│                         (44.1kHz stereo) every audio producer pushes into
│                         and Songbird pulls from; turn-gated clear
├── queue.rs                MediaSource (Spotify/YouTube/File) + QueueItem +
│                         PriorityQueue: the one ordered queue across sources
│
├── player/
│   ├── mod.rs             module doc: how state.rs and actor.rs divide work
│   ├── state.rs            pure decision core: PlayerState, Active (the
│   │                       turn), step(state, input, now) -> Vec<Effect>;
│   │                       imports no serenity/songbird/librespot-connect
│   └── actor.rs             impure shell: one task + mailbox owning
│                            PlayerState; PlayerHandle is the typed API
│                            (enqueue/skip/stop/toggle_pause/previous/query/
│                            lookup_spotify); runs media runners as spawns
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
│                             FNV-hash cache, fixed-gain bridge overlay
│
├── youtube/
│   ├── mod.rs                yt-dlp/ffmpeg availability checks, tmp-dir/
│   │                        cookies path resolution
│   ├── metadata.rs            fetch_youtube_metadata via `yt-dlp --dump-json`;
│   │                        duration cap, live-stream rejection, attachment
│   │                        validation
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
    │                          /np /announce, button routing
    ├── account.rs                /login /logout /forget, device-code poll,
    │                          account-switch bookkeeping, boot auto-start
    ├── ui.rs                     one task owning the now-playing/controls
    │                          card (UiMsg mailbox, one card_id)
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
- **Credentials.** `discord::account` reads/writes through `users::UserStore`,
  which encrypts token blobs via `users::crypto::TokenCipher`. Neither the
  player nor the Spotify session touches SQLite directly.

## Tests

No separate `tests/` directory — every test is an inline `#[cfg(test)] mod
tests` in the file it covers (`cargo test` runs them all). The pure core
carries most of the suite:

- `src/player/state.rs` — the bulk of the crate's tests: `step()` behavior
  per input/state combination (radio-order rules, arming/ack, pause
  provenance, timers, snapshots), deterministic since the core takes no IO.
- `src/discord/commands.rs` — voice-gate policy, Spotify link/track-id
  parsing, now-playing rendering.
- `src/config.rs` — env parsing (snowflake validation, numeric clamping,
  text-channel fallback).
- `src/users/mod.rs`, `src/users/crypto.rs` — credential store CRUD,
  encryption roundtrip, plaintext-downgrade rejection, wrong-key handling.
- `src/youtube/metadata.rs` — yt-dlp JSON mapping, duration cap, live-stream/
  age-limit handling.
- `src/audio_bridge.rs`, `src/discord/voice.rs` — buffer push/pull framing,
  prebuffer timing.
- `src/oauth/mod.rs`, `src/discord/presence.rs`, `src/audio/mod.rs`,
  `src/spotify/sink.rs` — smaller focused suites per module.
