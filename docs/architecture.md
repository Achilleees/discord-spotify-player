# Architecture

## Audio pipeline

```
Spotify (librespot decode) ─┐
YouTube/SoundCloud (yt-dlp) ─┼─> AudioBridge ─> SimpleBridgeReader ─> Songbird ─> Discord voice
uploaded files ─────────────┘         ▲
DJ TTS overlay ────────────────────────┘ (mixes on top at a fixed gain)
```

`AudioBridge` (`src/audio_bridge.rs`) is a lock-based `VecDeque<f32>` ring
buffer, 44.1 kHz stereo, shared between every producer and the one consumer.
It drops on overflow and drains/drops on even stereo frames so left/right
stay in sync. **Only the turn holder — the player actor — clears it**; the
Spotify layer never does, so a stale clear from the wrong source can't cut
audio out from under whoever currently holds the turn. On the Spotify side,
`src/spotify/sink.rs` (`DiscordSink`, librespot's audio backend) checks
`bridge.spotify_muted()` before pushing decoded audio in at all — the player
actor toggles that flag as part of taking or releasing the turn.

`SimpleBridgeReader` (`src/discord/voice.rs`) is Songbird's `Read + Seek +
MediaSource` over the bridge. On its first read only, it blocks until
`PREBUFFER_SECONDS` worth of samples have accumulated (or a timeout elapses)
so playback doesn't start on a buffer that's still filling.

## The player actor

One task owns all playback state — the queue, the armed Spotify track, and
the turn (who's entitled to be audible). Every command (slash commands,
buttons, the Spotify session, timers) reaches it as an `Input` through a
mailbox; the actor **awaits nothing, ever** — every effect it produces is
either a synchronous send/atomic-store or a `tokio::spawn` (media runners,
voice joins, announcements, timers). Asynchronous completions come back in
as their own `Input`s (`MediaEnded` tagged with an epoch, `VoiceReady`/
`VoiceLost`, `Tick`), so a stale completion is ignored by the pure core
rather than raced against a newer one.

The decision logic is a pure function, `src/player/state.rs`:

```rust
fn step(state: &mut PlayerState, input: Input, now: Instant) -> Vec<Effect>
```

It imports no serenity, songbird, or librespot-connect types — only `std`, a
plain `tokio::sync::oneshot` handle, `librespot_core::SpotifyUri`, and
`crate::queue` — so it's deterministic under test (most of the crate's unit
tests live here) and is the piece designed to port to `nob` unchanged.
`src/player/actor.rs` is the impure shell: it owns the mailbox, runs `step`,
and executes the returned `Effect`s in order.

### The turn: `Active`

```rust
enum Active {
    None,
    Media { item_id, item, paused, epoch },      // a queue item is playing
    SpotifyPending { uri, sent, retried },        // asked Spotify to start; awaiting Playing
    Spotify { track },                            // the Spotify baseline holds the turn
}
```

Turn changes only at boundaries the bot itself defines: media end, Spotify
`EndOfTrack`/`Stopped`, a human skip, or an explicit human play. No incoming
transport event moves the turn by itself — that's what keeps a DJ's phone
tap from silently stealing playback state out from under the queue.

## Three independent lifecycles

1. **Player** (`src/player/`) — the queue, the turn, what's audible. Reached
   only through `PlayerHandle`.
2. **Spotify session** (`src/spotify/session.rs`, `SessionSupervisor`) — the
   librespot task itself, its proactive token refresher, and a monotonic
   session generation. Started by `/login`, boot auto-start, or on demand
   (`ensure_session`) when a Spotify link is queued with the link down.
   **Imports neither songbird nor the queue** — its only surface into the
   player is `Input`, sent through the same mailbox as every other source,
   so an account switch mid-track is structurally unable to reach playback
   directly.
3. **Account** (`src/discord/account.rs`) — `/login` stores credentials,
   marks one account active in SQLite, and calls `SessionSupervisor::switch`.
   Nothing else. Switching accounts never touches the player: a media item
   already playing keeps playing straight through a login, and the actor
   drops the replaced session's armed track itself once the new session's
   `LinkUp` arrives.

Keeping these separate means a session dying (a dead refresh token, a
takeover, a forced disconnect) can never leave the queue or the turn in an
inconsistent state — the player only ever finds out via ordinary `Input`s
(`LinkUp`/`LinkDown`/`Transport`), the same path any other event takes.

## Task / mailbox topology

```
Discord gateway ──► discord::bot::Handler
                        │ PlayerHandle          │ SessionSupervisor
                        ▼                        ▼
                  ┌────────────┐         librespot session task
                  │player actor│  Input::Transport{gen,ev} ────┐
                  │(PlayerState)│◄────────────────────────────┤
                  └────────────┘  Input::LinkUp/Down/Reconnect │
                    │      ▲                                    │
   Effect::Spirc    │      │ Input::MediaEnded{epoch}           │
                    ▼      │                                    ▼
            spirc_cmd_tx ──┘                          Input::VoiceReady/Lost
                    │                                            ▲
                    ▼                                            │
              live Spirc session                     media runner (spawn/item)
                                                                  │
                                                                  ▼
                                        AudioBridge ─► SimpleBridgeReader ─► Songbird

  player actor ── Effect::Ui ───────► discord::ui task (owns one card_id)
  player actor ── Effect::Presence ─► discord::presence::run_presence_loop
  player actor ── Effect::Announce ─► DJAnnouncer (audio/dj.rs) ─► bridge overlay
```

Every arrow into the player actor is an `Input` on its one mailbox, so
decide-then-act is fully serialized — nothing can read `PlayerState`, decide
something, and write it back in a torn state, because only the actor's own
task ever touches it.

## Transport events and gen-tagging

`src/spotify/player.rs` translates raw librespot `PlayerEvent`s into
`TransportEvent`s (`Playing`, `Paused`, `Stopped`, `EndOfTrack`,
`Unavailable`, `TrackChanged`, `SetQueue`, `SessionConnected`/
`SessionDisconnected`) onto a channel; `transport_shim` in
`src/discord/bot.rs` wraps each one as `Input::Transport { gen, ev }` and
sends it to the player mailbox.
`gen` is the session generation `SessionSupervisor` assigned when it started
this particular librespot task (`switch` bumps a monotonic counter). The
player core compares an incoming generation against its own `link_gen`, so a
straggling event from a session that has since been replaced (an account
switch, a reconnect) is a silent no-op instead of corrupting state that
belongs to the new session.

## Arming and acknowledgement

While Spotify holds or is about to hold the turn, the actor arms the first
Spotify track anywhere in the queue by sending `SpircCmd::AddToQueue(uri)`.
Because librespot has no way to remove a queued track, arming is a one-shot,
advisory operation: the actor tracks its outcome as an `Ack`
(`Sent(Instant)` → `Confirmed` or `Lost`), and a `Lost` ack is never blindly
retried (that would double-queue the track). Confirmation comes from the
transport's own `SetQueue` event, which — filtered to `provider == "queue"`
entries — reports back exactly what `AddToQueue` created, distinct from
context/autoplay tracks Spotify queues on its own. An unacknowledged arm
past `ARM_ACK_TIMEOUT` (2s) is marked `Lost` rather than retried.

## Device activation

Claiming Spotify's "active device" slot is explicit-only: it happens on
`/login` (a human claim on the device) or on a human pressing ▶ to take over
(`Effect::Spirc(SpircCmd::ActivateDevice)`), and nowhere else — never on a
bare reconnect, never on boot auto-start, never on an on-demand session
brought up to resolve a queued link. This is deliberate: a background
session coming up (auto-start, `ensure_session`) must not steal the
"currently playing" slot away from a DJ who's actively using their phone.

## See also

`CODEMAP.md` (repo root) for a file-by-file map of `src/`.
