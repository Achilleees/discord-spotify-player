# Architecture

## Workspace and process entry point

The default workspace member is `discord-spotify-player`; its existing binary
calls `run()` in `src/lib.rs`. The `crates/nob` binary calls `run_nob()` in the
same library. Both use the same private music modules and run in separate
processes. Nob registers the additional server commands in `discord/admin.rs`.

`runtime.rs` selects the profile, loads an isolated configuration map and
resolves state paths before connecting. Process-held file locks protect the
database, Spotify cache/device identity, download scratch directory, cookie jar and DJ
cache from concurrent use by these hosts. Paths are frozen once for media
helpers. Older builds do not participate in these locks.

Startup initializes process-global logging once. `--check-config` exits before
state creation, dependency probes or network connections. See
[two-bots.md](two-bots.md) and [configuration.md](configuration.md).

## Routing and voice ownership

`routing/` defines typed actions and a loopback TCP transport. Nob's
`discord/front.rs` retains the Discord interaction and sends a request to
one performer; `discord/routing.rs` resolves it using that host's actor,
Discord cache and account owner. No raw interactions, bot tokens, Spotify
tokens or database handles cross the connection. Private menus bind user,
guild, room and process/session revision, expire after five minutes and
consume their token on each action.

Frames use the existing XChaCha20-Poly1305 dependency and a shared random key.
A fresh server challenge and direction-bound associated data prevent replay
across connections and request/response reflection. The transport caps frames
at 64 KiB, concurrent connections at 16, and the per-process request journal
at 512 entries retained for 15 minutes. New requests expire within a minute.
Accepted actions continue if a response waiter disappears; duplicates reuse
the recorded result, and conflicting payloads with the same ID fail. Normal
actions time out after a minute; device-pairing completion has an 11-minute
cap and four login slots. Restart loses the journal and changes the boot ID;
unknown outcomes never trigger automatic redispatch or performer fallback.

`discord/voice_owner.rs` reserves a room before spawning a join. Stop retires
that lease immediately; a serialized transition lock and lease checks keep
an old join greeting, bridge attachment or departure from touching a newer
connection. An admin move updates the routing revision while retaining the
connection's audio lease. The player core separately correlates join results
by generation, ignoring late success/failure after Stop or a replacement.
Guarded player requests recheck current voice membership and routing revision
at mailbox consumption. Account joins also reserve against their original
revision, and delayed device activation passes through the same actor guard.
Future soundboard visits must extend this owner with activity-specific rules.

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

### The two paths do not arrive at the same level

The producers reach the bridge through different amounts of gain, and
nothing reconciles them:

| Path | Gain before the bridge |
|---|---|
| Spotify | librespot's soft mixer, then `DiscordSink`'s DSP |
| yt-dlp / files | none at all — decoded samples go straight in |

The mixer is the larger and less obvious half. `ConnectConfig.initial_volume`
reads like a display value, but `Spirc::new` hands it to the soft mixer, and
the default `VolumeCtrl::Log(60 dB)` maps our 80% (`52428`) to an amplitude
of 0.2512 — **exactly -12 dB**, applied to every Spotify sample. `PREAMP_DB`
then applies on top (`-5 dB` on the current test host, so -17 dB total there,
partly masked by a `+7 dB` shelf at 80 Hz that puts low-end energy back).

That is why YouTube and SoundCloud are audibly hotter than Spotify. A fixed
counter-attenuation on the media path would only match at one slider
position: the mixer *is* the DJ's volume control, so every time they move it
on their phone the two paths drift apart again. Making the media path follow
the same mixer volume is the fix that stays correct; it is not implemented.

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
and `false` from the teardown paths, which own the voice connection's fate
themselves — it is already gone on a force disconnect, and the caller removes
it on an empty channel. Either way no echo is coming. A teardown that armed
the guard would leave it latched — Discord sends no echo for a state that did
not change — and the next genuine force disconnect would read as deliberate,
leaving librespot feeding a dead call.

The guard counts outstanding departures rather than flagging one, so that
undoing an arming can only ever undo its own. As a shared flag, a second
`/stop` racing the first one's echo cleared the *first* one's arming, and
that echo was then read as a force disconnect.

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
`Effect::ResolvePrevious { before }`. The actor queues that read on the same
worker as `RecordAired`, behind earlier history writes, so a Back press just
after a track starts sees its row. The answer returns through the mailbox as
`Input::PreviousResolved`; the actor never waits for the database. The read
streams older rows in descending id order until it finds a Spotify reference,
skipping media and invalid references without a fixed row-count cutoff.

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
tracks forever; the cursor clears when playback moves beyond the walk,
which is what "we're live again" means. And the
arrival is checked (`awaiting_jump`): librespot silently starts a context
at track 1 when it cannot find the track requested, so a mismatch is
surfaced instead of quietly restarting the playlist.

Rapid taps can leave several context loads in flight. The core remembers up
to eight requested targets for five seconds, including completed ones:
superseded or duplicate arrivals still update playback telemetry, but neither
fail the newest jump nor reset its cursor. The newest target takes precedence
when history rows share a URI. This is bounded URI correlation, not command-id
matching: a genuine replay of a remembered target within that window also
looks like an echo. Unknown mismatches still warn immediately; a jump with no
confirmed arrival times out, reports that fact, and resets the walk. Timers
revalidate the newest send time, and commands also prune expired memory.

At the eight-target limit further taps receive "Already going back" until
space expires. Context-less `Previous` cannot name its arrival, so it waits
until context echoes expire and cannot itself be overlapped. Stop, session
loss/switch, skip, and a new media or explicit Spotify load clear the jump
memory. Movement uses the transport mirror, not the card's last track:
`TrackChanged` can update the latter before `Playing` arrives.

## librespot facts the design rests on

Comments through `src/` cite these by number. All five are verified against
the pinned revision (`1599145`) — re-verify before bumping it, since each
one is load-bearing and a silent change upstream would not fail a test here.

**F2 — a command to a device that isn't active is dropped, not queued.**
`spirc.rs:727` matches `_ if !self.connect_state.is_active()` and only
warns ("will be ignored while Not Active"). So an unacked `add_to_queue` is
*void*, and every audibility command has to be gated on `device_active`.

**F4 — `pause()` then `next()` is a silent advance.** `handle_next` opens
with `let continue_playing = self.connect_state.is_playing()`
(`spirc.rs:1717`), so from a paused state the next track loads paused at
0:00. That is how a human skip onto a media item consumes exactly one
Spotify track without a blip of it becoming audible.

**F12 — a transfer restores the queue along with everything else.** The
transfer payload carries `queue.tracks` and `is_playing_queue`
(`state/transfer.rs`), which is why reconnect prefers `Transfer(None)` over
re-activating: it brings the armed track back rather than starting clean.

**F15 — activation must be explicit.** Both `Transfer` and `Activate` are
ignored when the device is already active (`spirc.rs:711-725`), and an
unconditional `activate()` on connect would claim the device away from the
DJ's phone on every session start. This crate therefore never activates on
connect; only `/login`, ▶, or a queued Spotify item reaching its turn does.

**F16 — `previous` is not a promise to change track.** `handle_prev`
branches on position (`spirc.rs:1747-1763`): under 3000 ms it steps to the
previous track, at or over that it seeks to zero and keeps playing the same
one. So the context-less back-jump fallback cannot assume it moved — it
carries the cursor on the jump and commits it only when a *different* track
arrives, or a press three seconds into a song would advance the walk without
moving.

## See also

`CODEMAP.md` (repo root) for a file-by-file map of `src/`.
