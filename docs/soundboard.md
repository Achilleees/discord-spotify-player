# Nob's soundboard

`/soundboard` opens a private menu of local clips. Join a voice call, choose
a sound, and an available nob joins, plays it and leaves. The command belongs
to nob in both standalone and paired mode. Spotibot can keep playing music
while nob visits the same room or a different one.

## Using the menu

The picker shows ten sounds per page, with **Previous**, **Next**, **Refresh**
and **Close** buttons. Only the requester sees the menu. A panel expires after
five minutes and each accepted click replaces its single-use token, so a
double click cannot start the same visit twice. Expired menus can be reopened
with `/soundboard`.

Sound selections belong to the requester, guild, original voice room and
nob's voice revision. Paging preserves that binding; **Refresh** captures the
current room and revision. Refresh after moving rooms or after nob changes
voice activity. Opening the menu outside voice is allowed, but playing a
sound requires being in a voice channel other than the server's AFK channel.

An empty catalogue shows that no sounds are available. If nob already owns
a voice session, sound buttons are disabled with a refresh prompt. Active
or paused music on nob keeps him busy, including music in the requester's
room. A clip never interrupts that music. A music claim that takes priority,
including incoming Spotify playback, cancels an active visit. Leaving or
changing the requester's room, or an administrator moving nob, also cancels it.

**Close** closes the picker. During a visit the panel shows progress and then
the result with fresh controls. This feature does not receive voice audio,
listen for reactions or schedule random visits.

## Add local clips

Install `ffmpeg` on nob's service `PATH`. The soundboard does not require
yt-dlp or a Spotify login. Audio is supplied locally by the server operator;
the Discord menu accepts no uploads, URLs or filesystem paths. Keep write
access to this directory limited to the operator and the bot's service user.
Path checks do not make a directory safe to share with untrusted writers.

1. Create a `soundboard` directory beneath nob's state directory. With the
   defaults, this is `.nob/soundboard` under the process working directory.
2. Put short audio files in that directory, or in its subdirectories.
3. Create `.nob/soundboard/catalogue.json` using this format:

   ```json
   {
     "clips": [
       { "id": "hello", "label": "Hello!", "file": "hello.wav" },
       { "id": "tiny-fanfare", "label": "Tiny fanfare", "file": "effects/fanfare.mp3" }
     ]
   }
   ```

   Include only entries whose files exist. The array order is the menu order.
4. Restart nob to load the catalogue, then open `/soundboard` in Discord.

To use a different directory, put `SOUNDBOARD_DIR=your-directory` in `.env.nob`,
or set the process variable `NOB_SOUNDBOARD_DIR`. Relative paths resolve
beneath `STATE_DIR`; absolute paths stay absolute. The default remains
`soundboard` beneath that state directory. `--check-config` resolves the
setting offline; normal startup reads and validates the catalogue. Restart
nob after changing catalogue entries, labels or order.

For a small demo, this command creates a quiet, faded tone after you have
created the default directory. Use only its `hello` entry in the JSON above:

```sh
ffmpeg -f lavfi -i "sine=frequency=660:duration=0.3" -af "afade=t=in:d=0.01,afade=t=out:st=0.25:d=0.05" -c:a pcm_s16le .nob/soundboard/hello.wav
```

## Catalogue limits

| Item | Limit |
|---|---|
| Manifest | UTF-8 JSON, at most 64 KiB and 128 clip entries; unknown fields are rejected. |
| `id` | Unique, 1–32 ASCII letters, digits, underscores or hyphens. |
| `label` | 1–60 printable characters after trimming; no control characters. Long button labels are clipped to Discord's limit. |
| `file` | A relative path inside the soundboard directory; regular local files only, at most 20 MiB each. |
| Decoded audio | At most 15 seconds; longer clips are rejected, not trimmed. |

Supported local formats include AAC, AIFF, FLAC, Matroska/WebM, MOV/MP4, MP3,
Ogg and WAV. Network protocols and playlist formats are disabled. Absolute
paths, parent traversal, URLs, symlinks and Windows junctions are rejected
for clip files. Missing directory or manifest means an empty soundboard.
A present but invalid manifest prevents nob from starting, so fix the local
catalogue before restarting. Files are checked again when selected;
unavailable, unsupported or oversized audio produces a private error without
exposing local paths or decoder output.

## Playback and recovery

A visit reserves nob's voice ownership before decoding or joining. It uses
its own Songbird track at 0.5 gain, without the normal music join greeting,
queue insertion or bridge overlay. Playback success comes from the track's
end event; errors and timeouts report failure. Decoding is bounded to ten
seconds, joining to twelve seconds, and the visit to forty seconds overall.
After voice setup, nob waits 1.5 seconds before playing, then stays for two
seconds after a successful clip finishes. These pauses release the voice
transition lock and cancel immediately if the requester moves or music
takes over. Failures skip the departure pause.
Cleanup then attempts to leave only the connection the visit still owns.

Discord's native join/leave notification is separate from the clip. The
[supported voice API](https://docs.discord.com/developers/events/gateway-events#update-voice-state)
has no silent-join option. On desktop, each listener can disable **User Join**
and **User Leave** in **User Settings > Notifications > Sounds**; this applies
to their join/leave notifications generally, not just nob. See
[Discord's notification guide](https://discord.com/blog/how-to-manage-your-discord-desktop-notifications).

Music has priority over temporary visits. If music replaces the visit's
lease, late clip completion cannot stop or disconnect the music session.
If leaving fails, the clip stops and one background task retries bounded
cleanup attempts. Nob stays busy until removal or disconnect is confirmed,
or music takes over; the response asks users to check the connection.

Local tests cover catalogue bounds, decoding, private menu behavior and
ownership decisions. Discord playback, cancellation and two-bot behavior
still need live acceptance. Deployment remains a separate promotion of a
green `dev` commit to `main`.
