# Commands and controls

## Voice-channel gate

Most playback commands require you to share the bot's voice channel:

- If the bot **is** in a channel, you must be in that same channel.
- If the bot is in **no** channel, `/play` alone will follow you in — you
  just need to be in some voice channel, and the bot joins it.
- Buttons on the now-playing card require sharing the bot's channel, except
  the "➕ Queue" hint button, which is read-only and always available.
- `/announce` is a guild-level toggle, not playback control, and is ungated
  so it can be set before the bot has joined voice.

Failing the gate gets you: *"You must be in the bot's voice channel to
control playback."* (or, for `/play`'s fresh-boot path: *"Join a voice
channel first (or the bot's channel if it's already in one) to queue
playback."*)

## Slash commands

| Command | Who may use it | What it does |
|---|---|---|
| `/login` | Anyone; taking over someone else's active session requires sharing the bot's voice channel | Connects your Spotify account via device-code pairing, or quickly reactivates a stored session. Replies with a pairing link/code, then (after you approve) confirms the session started. |
| `/logout` | Anyone (only the session owner's logout stops playback) | Deactivates your Spotify session. Credentials are kept for a quick `/login` later. A bystander's `/logout` only affects their own row. |
| `/forget` | Anyone (only the session owner's forget stops playback) | Permanently deletes your stored credentials. If you're the active session owner, this also ends the live session. |
| `/who` | Anyone | Shows the Discord name behind the currently active Spotify session. |
| `/play [url] [file] [next]` | Voice gate (follow mode when the bot isn't in voice yet) | Starts playback if nothing is playing, otherwise enqueues. `next:true` jumps the queue (or lands right behind an already-armed head, since an armed Spotify track can't be un-queued). Accepts a Spotify/YouTube/SoundCloud URL or a file attachment, never both. Disabled at registration if yt-dlp/ffmpeg aren't available (Spotify links still work). |
| `/queue [url] [file]` | Voice gate | Always enqueues, never starts playback. With no argument, shows the current queue listing instead. |
| `/skip` | Voice gate | Skips the current track (media item or Spotify track) — see "How playback is ordered" below. |
| `/stop` | Voice gate | Stops playback and clears the queue. |
| `/np` | Anyone | Shows what's currently playing. |
| `/announce` | Anyone, ungated | Toggles DJ track announcements on/off; the setting persists across restarts. |

## Now-playing card buttons

| Button | Gate | What it does |
|---|---|---|
| ⏮ (`ctrl_prev`) | voice gate | Same as Spotify's "previous"; unavailable while a queue (media) item holds the turn. |
| ⏯ (`ctrl_pause_toggle`) | voice gate | Pauses/resumes the active media item, pauses a playing Spotify baseline, or — if nothing is audible — starts/resumes whatever the queue head or Spotify state implies (see below). If the device isn't active yet, this is also the takeover gesture. |
| ⏭ (`ctrl_next`) | voice gate | Same as `/skip`. |
| ➕ Queue (`ctrl_queue_hint`) | none — always available | Ephemeral reply with the queue listing and how to add to it. |

Button replies are ephemeral (only the clicker sees them), so they never
spam the channel.

## How playback is ordered

**Radio rules.** One queue holds Spotify tracks, YouTube/SoundCloud tracks,
and file uploads together, in the order they were added — the source
doesn't create separate lanes. Tracks play strictly in that order. The bot
**never** sends Spotify a `Next` on its own; the only way past a track is a
human pressing ⏭ or running `/skip`. Priority when more than one thing could
be audible: a DJ announcement overlay (if enabled) plays over whatever else
is going, then the queue, then the Spotify Connect baseline (whatever's
already playing on the DJ's Spotify session when nothing is queued).

**One armed Spotify track.** While Spotify holds (or is about to hold) the
turn, the player arms the first Spotify track anywhere in the queue into
Spotify's own device queue (`AddToQueue`), so librespot's own end-of-track
advance lands on it automatically. Any queue items ahead of it (YouTube
tracks, files) play first through the bot's own media path; Spotify sits
paused on the armed track until it's their turn. The armed marker in
`/queue`'s listing (⏭ next on Spotify) shows this.

**DJ pauses/plays from their phone mid-queue.** The player tracks who
caused a pause (bot-for-media, bot-for-stop, or human) so it knows whether
to auto-resume later. A human pause on the Spotify side is honored and
never silently overridden, but it also never blocks an explicit Discord
command (⏯, `/skip`, `/stop`) from taking over. If the DJ skips ahead from
their own phone, the player detects the matching `Playing` event and
reconciles the armed track instead of getting confused about what's
airing.

**`/stop` semantics.** Clears the queue and silences whatever's audible: a
playing media item is cancelled, or a playing Spotify baseline is paused.
One caveat — Spotify has no way to un-queue a track once it's been armed
(`AddToQueue`), so if a track was already handed to Spotify at the moment
you stop, it may still play once when Spotify's own auto-advance reaches
it. The reply says so explicitly: *"⏹ Stopped. Queue cleared. (a track
already handed to Spotify will still play once)"*.

**`/play` vs `/queue`.** `/play` starts playback immediately if nothing is
playing; otherwise it behaves like `/queue` (optionally jumping the line
with `next:true`). `/queue` always adds to the tail (or head hint for
Spotify's fast path, which — since Spotify links skip the yt-dlp probe —
still just enqueues) and never starts playback on its own, even if nothing
is currently playing. Both accept the same inputs: a Spotify track
URL/URI, a YouTube/SoundCloud URL, or an audio file attachment (mp3, flac,
ogg, opus, wav, m4a, aac, wma; 50 MB max).
