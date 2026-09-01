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

The separation cuts the other way too: `/stop` belongs entirely to (1). It
pauses, releases the Connect device, leaves the voice channel and keeps
the queue — and never reaches the session supervisor or the account. That
needs one guard, because leaving voice is not a purely local act: Discord
echoes the departure back as a voice-state update, and the handler treats
an unexplained one as a force-disconnect worth tearing the session down
for. A `leaving_voice` flag, set immediately before `manager.remove`,
marks our own departure as deliberate so a stop cannot log the DJ out.

That guard is armed by whoever asks to leave, so only a departure that
Discord will actually echo may arm it. `Input::Stop` carries a
`leave_voice` flag for exactly this: `true` for a human `/stop`, where the
voice gate guarantees the bot is in the channel and the echo always comes,
and `false` from the teardown paths, which run *because* voice is already
gone. A teardown that armed the guard would leave it latched — Discord
sends no echo for a state that did not change — and the next genuine force
disconnect would read as deliberate, leaving librespot feeding a dead call.

The mirror rule governs joining. Audio reaches Discord only through the
bridge, and the bridge is drained only by a live call, so every path that
makes the bot audible has to arrange one. `ensure_voice` is that single
point: `start_media` calls it, `begin_load` calls it, both ⏯ resume arms
and the takeover call it, and so does a turn-approved `Playing` — our own
librespot decoding is proof audio exists, whoever started it. The one ▶
outcome that makes no sound, "Nothing is playing right now", deliberately
does not. Joining is idempotent (the shell short-circuits when already in
a call, answering `VoiceReady` at once), which is also what lets the core's
`voice` field heal itself when the shell joined without telling it.

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

Claiming Spotify's "active device" slot is tied to a request, never to a
session coming up. It happens (`Effect::Spirc(SpircCmd::ActivateDevice)`)
on `/login` (a human claim on the device), on a human pressing ▶ to take
over, and when a queued Spotify track reaches its turn — queuing it *was*
the request to hear it here. It never happens on a bare reconnect, on boot
auto-start, or on an on-demand session brought up to resolve a link: a
background session must not steal the "currently playing" slot away from a
DJ who's actively using their phone when nothing has been asked of the bot.

## What aired, and what is still queued

Two tables in the same SQLite file as the credential store, with one job
each.

**`play_history`** is append-only and written when a track *becomes
audible* — never when it is queued. Spotify's own playlist and autoplay
tracks are recorded exactly like requests, because the bot drives the
account: this log, not Spotify's, is the record of what the room heard.
Every row keeps the `context_uri` it aired from, which is what makes a
back-jump possible. The core emits `Effect::RecordAired` alongside the
card that announces the track, so a track paused straight back down under
a media item — one that never actually played — writes nothing.

**`queue_items`** holds the pending queue and is rewritten whole inside one
transaction whenever `PriorityQueue::revision()` changes. The actor
compares that counter across each `step`, so the write happens on real
queue mutations rather than on every transport event. There is no expiry:
nothing is audible without a voice channel and people in it, so presence
already gates a stale queue, and the VPS redeploys on every push to `main`
— restarts are routine, not exceptional. `Input::RestoreQueue` refills the
queue at boot, stamps fresh item ids, starts nothing, and refuses to
displace a queue someone is already using.

## Going back

⏮ walks the bot's own history rather than asking Spotify, because the two
diverge the moment anyone touches a phone. The core stays pure: it emits
`Effect::ResolvePrevious { before }`, the actor does the blocking read on
a worker thread, and the answer returns through the mailbox as
`Input::PreviousResolved`.

Playing the result uses `SpircCmd::LoadContext`, which becomes
`LoadRequest::from_context_uri(..., playing_track: Uri(target))` — it
*reopens* the playlist positioned at the track. That is a different
operation from `SpircCmd::Load`, which builds a one-track context and so
replaces the DJ's playlist; `Load` stays restricted to `sp == Idle` for
exactly that reason. Two properties of librespot shape the rest:

- A context load does **not** discard Spotify's queue
  (`clear_next_tracks` is queue-preserving), so an armed request survives
  a back-jump — it simply plays after the track jumped to.
- A context load **does** reset shuffle and repeat, so the jump carries
  the DJ's options back with it (`PlayOptions`, mirrored from Spotify's
  `ShuffleChanged`/`RepeatChanged` events).

Two guards fall out of that. The walk carries a cursor (`history_cursor`)
rather than re-reading "the second-newest row", because replaying a track
appends a row of its own and the naive query would bounce between two
tracks forever; the cursor clears itself as soon as something other than
the jump target plays, which is what "we're live again" means. And the
arrival is checked (`awaiting_jump`): librespot silently starts a context
at track 1 when it cannot find the track requested, so a mismatch is
surfaced instead of quietly restarting the playlist.

## See also

`CODEMAP.md` (repo root) for a file-by-file map of `src/`.
