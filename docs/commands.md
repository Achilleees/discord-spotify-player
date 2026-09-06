# Commands and controls

Both bots share the music controls. Their slash surface depends on
`COMMAND_MODE`; see [paired setup](two-bots.md#one-slash-menu-for-both-bots).

## Paired commands

| Nob command | Opens or does |
|---|---|
| `/play query:… file:… next:…` | Plays a link, attachment or song search in your voice room; omit input to start/resume. |
| `/music` | Private performer picker and playback, queue/history, account and announcement controls. |
| `/soundboard` | Private, paginated clip picker; free nob visits your voice room, plays the selected sound and leaves. |
| `/server` | Private slowmode and message-cleanup tools for this channel. |

Spotibot has no slash entries in worker mode, but its card buttons continue
working. Nob also keeps `/soundboard` in standalone mode.
The private panel names its performer; both bots retain independent queues
and Spotify accounts. An expired panel or changed voice session requires a
fresh `/music`. Clear queue and Forget login have confirmation buttons.

The individual slash commands documented below remain available in explicit
standalone mode. In paired mode use the matching action in `/music` or `/server`.

## Nob soundboard

`/soundboard` is private and nob-only in both command modes. It shows ten
local clips per page, with Previous, Next, Refresh and Close. Panels expire
after five minutes; each accepted click consumes its token. Clip selections
bind to the requester, guild, original voice room and nob's voice revision.
Page changes preserve that binding; Refresh updates it after voice changes.

Join a non-AFK voice channel to select a sound. Free nob can visit while
Spotibot keeps playing. Nob's own music, including paused music, makes him
busy and prevents a clip visit. A music takeover or requester/admin voice
move cancels an active visit. Busy and empty menus explain their state;
no voice audio is received and no visits happen randomly. See
[soundboard.md](soundboard.md) for setup and playback details.

## Nob server utilities

| Command | Required channel permissions (caller and bot) | Behavior |
|---|---|---|
| `/slowmode seconds` | Manage Channels | Sets text/voice channel slowmode from 0 to 21600 seconds; 0 disables it. Threads are not supported yet. |
| `/purge count` | Manage Messages, View Channel, Read Message History | Inspects 1-100 recent messages and deletes eligible ones. Preserves pins, bot messages, and messages within two minutes of Discord's 14-day bulk-delete limit or older. |

These tools exist only on nob (in `/server` when paired), are limited to its configured guild and
reply privately. They use server permissions independently of voice membership.
Cleanup preserves all bot/webhook messages because Discord may omit their
[button data](https://docs.discord.com/developers/resources/message#message-object)
without the Message Content intent. It reports confirmed outcomes; failures
do not report success.

## Voice-channel gate

Music controls require you to share the performer's voice channel:

- If the bot **is** in a channel, you must be in that same channel.
- If the bot is in **no** channel, `/play`, **Add music** and the idle **Play**
  button can follow you in — you just need
  to be in some voice channel, and the bot joins it.
- Previous, Pause/Resume, Skip and Stop require sharing the bot's channel.

Soundboard selections use the separate idle-visit rules above: nob must be
free and follows the requester into their non-AFK voice channel.

`/clear` and its confirmation require the bot's channel while it is connected,
or any voice channel while the bot is out of voice. That covers the confirm
button, and it exists because `/stop` leaves the channel while keeping the
queue — under the strict gate, the command `/stop` tells you to use would be
refused in exactly the state `/stop` creates.

Reads and settings are ungated: `/np`, `/history`, `/who`, the private Queue
and History buttons, and cancelling the clear prompt. The `/queue` slash
command retains its voice gate even when showing the listing.
`/announce` is a guild-level toggle rather than playback control, so it can be
set before the bot has joined.

Failing the gate gets you *"You must be in the bot's voice channel to control
playback."*, or *"You must be in a voice channel to change the queue."* for
the queue-only actions (or, for `/play`'s fresh-boot path: *"Join a voice
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
| `/stop` | Voice gate | Stops playback, releases the Spotify device and leaves the voice channel. The queue is kept. |
| `/clear` | In any voice channel (the bot's, if it is in one) | Empties the queue after a confirmation prompt. Whatever is playing keeps playing. |
| `/history [count]` | Anyone | Lists what has actually aired, newest first (1–25, default 10), each stamped with the time it played in your own timezone. Requests name whoever asked for them; the DJ's own playlist tracks don't. |
| `/np` | Anyone | Shows what's currently playing. |
| `/announce` | Anyone, ungated | Toggles DJ track announcements on/off; the setting persists across restarts. |

## Now-playing card buttons

| Button | Gate | What it does |
|---|---|---|
| Previous (`ctrl_prev`) | voice gate | Steps back through the bot's own play history. Reopens the playlist the track came from, positioned at it, so the DJ's context survives. Unavailable while a queue (media) item holds the turn. |
| Pause / Resume (`ctrl_pause_toggle`) | voice gate | Pauses/resumes playback; its label and the card's paused state stay in sync through account changes. |
| Skip (`ctrl_next`) | voice gate | Same as `/skip`. |
| Stop (`ctrl_stop`) | voice gate | Same as `/stop`; keeps the queue and leaves voice. |
| Play (`ctrl_play`, idle card) | voice gate with follow mode | Same as bare `/play`: resumes/starts available playback without toggling it off. |
| Add music (`ctrl_add_music`) | voice gate with follow mode | Opens the track-link / YouTube search modal described below. |
| Queue (`ctrl_queue_hint`) | none | Private queue listing, including the armed Spotify marker. |
| History (`ctrl_history`) | none | Private listing of the ten most recently aired tracks. |
| Clear the queue (`ctrl_queue_clear_confirm`) | same as `/clear` | Confirms `/clear`. Rechecks voice membership when clicked. |
| Cancel (`ctrl_queue_clear_cancel`) | none | Dismisses the `/clear` prompt without touching the queue. |

Button replies are ephemeral (only the clicker sees them), so they never
spam the channel.

## Add music and search

Click **Add music** and enter a song/artist name or a Spotify, YouTube or
SoundCloud track link. A link follows the same request path as `/play`.
Text searches YouTube and shows up to five choices in a private message.
Click a numbered button to add that track to the queue; playback starts
only if idle. Search is available without a Spotify account when yt-dlp and
ffmpeg are installed. Spotify links still need a connected account.

Choices expire after five minutes and accept one selection. Open Add music
again to make another request. Voice membership and media availability are
checked again after a slow lookup, so moving away cannot enqueue into the
room you left. Lookup errors and result selections stay private.

The public card shows the source, artwork and paused state. Its owner refreshes
it periodically and recreates a deleted card. Progress, seeking, EQ and saved
playlist controls are subsequent features and are not displayed yet.

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
caused a pause (bot-for-media, or human) so it knows whether
to auto-resume later. A human pause on the Spotify side is honored and
never silently overridden, but it also never blocks an explicit Discord
command (⏯, `/skip`, `/stop`) from taking over. If the DJ skips ahead from
their own phone, the player detects the matching `Playing` event and
reconciles the armed track instead of getting confused about what's
airing.

**`/stop` semantics.** Stop is stop, not pause: a playing media item is
cancelled, the Spotify device is paused and released, and the bot leaves
the voice channel. It stays inside the player's own lifecycle — the
Spotify session and the account are untouched, so a stop never logs
anyone out.

The queue survives. `/clear` is the only thing that empties it, and it
asks first: an ephemeral prompt with Confirm/Cancel, whose buttons are
removed once answered so it cannot be answered twice. One caveat applies to
`/clear` alone: Spotify has no way to un-queue a track once it has been armed
(`AddToQueue`), so a track already handed over may still play once when
Spotify's own auto-advance reaches it. `/clear` says so explicitly when that
is the case. `/stop` normally is not affected: releasing the device resets
Spotify's own queue along with it, so the arm goes too. The exception is a
`/stop` with the device already released, which sends no release and so
leaves the armed track exactly where it is.

Because `/stop` leaves the channel, `/clear` permits a requester in any voice
channel while the bot is out of voice. While the bot is connected, it still
requires sharing that channel.

**`/play` vs `/queue`.** `/play` starts playback immediately if nothing is
playing; otherwise it behaves like `/queue` (optionally jumping the line
with `next:true`). `/queue` always adds to the tail (or head hint for
Spotify's fast path, which — since Spotify links skip the yt-dlp probe —
still just enqueues) and never starts playback on its own, even if nothing
is currently playing. Both accept the same inputs: a Spotify track
URL/URI, a YouTube/SoundCloud URL, or an audio file attachment (mp3, flac,
ogg, opus, wav, m4a, aac, wma; 50 MB max).
