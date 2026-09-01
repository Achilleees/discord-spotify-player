# Changelog

This is a Discord music bot that mirrors a Spotify Connect session into a
voice channel and can also pull in tracks from YouTube, SoundCloud, and
uploaded files, all controlled from Spotify itself, from Discord slash
commands, and from now-playing buttons. Each entry below corresponds to a
git tag (release) in this repository.

---

## Unreleased

**The queue survives restarts, and the bot keeps a record of what it played.** Requests no longer vanish when the bot restarts — which happens routinely, since it redeploys on every update. There is deliberately no expiry: nothing plays unless the bot is in a voice channel with someone in it, so a queue left over from last night simply waits rather than going stale, and it never starts playing on its own when the bot comes back. Alongside that, every track that actually airs is now written to a play history, including the ones Spotify chose itself, with the playlist it came from — groundwork for looking back at what was played on a given evening.

**`/history` shows what has actually played.** Newest first, with the name of whoever asked for a track next to it so requests read apart from whatever the DJ's own playlist reached on its own. Takes a count between 1 and 25, and trims itself rather than being rejected outright when the titles run long.

**Going back now actually goes back.** The ⏮ button steps through what this bot played, rather than asking Spotify what it thinks came before — the two drift apart the moment anyone touches their phone. Going back to a track reopens the playlist it came from, positioned at that track, so the DJ's listening context survives instead of being replaced by a single song. Pressing it repeatedly keeps walking backwards rather than bouncing between the last two tracks, and if Spotify can't find the track in that playlist the bot says so instead of quietly restarting the playlist from the top.

**`/stop` now means stop, and `/clear` is what empties the queue.** Stopping pauses Spotify, hands the device back and leaves the voice channel, but leaves the queue intact so nobody's requests are lost by someone reaching for the wrong command. Emptying the queue is now its own command, and it asks before it acts — a private prompt with Confirm and Cancel, which disappears once answered so it can't be triggered twice. Stopping also no longer risks logging the DJ out: the bot recognises its own departure from voice instead of mistaking it for being kicked.

**The bot always joins the voice channel before it starts making sound.** Previously only queued YouTube, SoundCloud and file tracks pulled the bot into the channel; anything coming from Spotify assumed it was already there. Since stopping now leaves the channel, that assumption broke in the obvious way — pressing play again, or starting a track from your phone, would show a now-playing card while the channel stayed silent, and the only way back was to log in again. Every route to audio now brings the bot in first, and being pulled out unexpectedly no longer leaves the bot unable to recognise the next genuine disconnection.

**Audio no longer drifts behind, and a failed track can't cut off the one that's playing.** Two faults found by listening rather than by testing: a track Spotify failed to load in advance could dump the audio buffered for the song actually playing, and once that buffer filled up it stayed full — leaving Discord running about ten seconds behind Spotify with no way to recover short of a restart. Both are fixed; playback now catches back up to live instead of accumulating delay.

**Spotify playback works again after Spotify blocked third-party apps from streaming.** In August 2026 Spotify cut off playback for apps using their own client identity, which took the bot's Spotify Connect session down entirely. It now signs in the same way Spotify's own desktop app does, so playback keeps working without the bot needing its own registered client anymore. The web-based Spotify API has been dropped completely — track titles, artist and album info, and playback control now all come from the same connection used for streaming, so there's one less thing that can fall out of sync or need re-authorizing.

**`/play` and `/queue` are now the same thing, and playback always follows the queue in order.** Adding a track — whether it's a Spotify link, a YouTube or SoundCloud link, or an uploaded file — no longer jumps the line or interrupts what's already playing; everything lines up and plays in the order it was added. The bot never skips a track on its own anymore; only an explicit skip (the button or `/skip`) advances early. When Spotify's own queue is next in line, the bot arms the upcoming Spotify track in advance so the handoff back to Spotify Connect is seamless instead of stalling or replaying.

**Logging in no longer starts playback by itself.** `/login` now only stores and activates an account; the Spotify session that actually streams audio starts on its own, either automatically at boot for the last active account or on demand the first time a Spotify track is queued. This removes a class of bugs where logging in while someone else was mid-session could interrupt them.

**Playback logic was rebuilt around a single, predictable decision-maker.** All commands, button presses, timers, and the Spotify session itself now funnel through one internal component that decides what plays next, rather than several parts of the bot independently reaching for control of playback. The practical effect is fewer of the glitches this design change specifically targeted: tracks that silently failed to advance, queued items that got dropped at a track boundary, skips that didn't register, and audio that got cut or cleared out from under a session that still owned it.

**The bot no longer hijacks your phone's Spotify playback when a session starts.** Previously every session start silently made the bot the active Spotify device, cutting off whatever you were listening to. Now the bot becomes the playing device only when you ask — on `/login`, by pressing ▶ in Discord, or by picking it in Spotify's device list — and it reports its volume as 80% when it first appears instead of 50%.

**Pressing play on your phone while a queued YouTube/SoundCloud track is airing no longer glitches the audio.** Spotify is paused right back as before, but its first few samples never reach the voice channel and the queued track's buffer is left untouched. A missed Spotify handoff (the bot's queued track not landing after a skip) no longer wedges the queue: the bot notices and re-queues it, so the next skip works. The now-playing card also follows a skip made while paused, so it shows the track that is cued up rather than the one that just ended.

**Various reliability fixes landed alongside the rework.** Tracks Spotify refuses to play are reported clearly instead of hanging, `/forget` now ends any session it removes credentials for, and the DJ's shown name is their Discord name. Automated build checks (linting, tests, and a release build on every change) were added, a security and correctness review of the whole codebase was completed and its findings closed out, and the automated test suite roughly doubled.

## v0.5.0-rc2 — 2026-07-10

**Expired logins now offer to fix themselves instead of dead-ending.** If a stored Spotify login could no longer be refreshed, the bot used to just fail; now it recognizes the situation and walks the user through logging in again rather than leaving them stuck.

**YouTube playback got more reliable file handling and could now play some age-restricted videos.** Cache and cookie file locations now default correctly for the bot's normal server setup, and videos that are age-gated on YouTube can play when a cookie file is supplied to unlock them, rather than failing outright.

## v0.5.0-rc1 — 2026-07-10

**YouTube, SoundCloud, and local file playback arrived alongside Spotify.** `/play` now recognizes what kind of link (or search term) it's given and queues the right kind of track, mixing Spotify, YouTube, SoundCloud, and uploaded files freely in the same queue.

**A DJ announcer can now speak track intros using text-to-speech.** This is a separate, optional overlay that can announce what's coming up without interrupting the underlying queue.

**Login and account storage got substantially more secure.** Spotify sign-in moved to the more secure PKCE variant of OAuth with a hardened paste-back step, and stored credentials moved from a plain file into an encrypted database, with the encryption key strengthened against brute-forcing and tied to the account it protects so a credential can't be swapped between accounts. Sign-ins also got a proactive token refresh, so a session is far less likely to silently expire mid-use, and a race where two logins at once could corrupt state was closed.

**Voice playback moved onto Discord's official, stable encrypted-voice support**, replacing the community fork of the voice library that earlier releases depended on for the same feature. Audio playback itself also got more robust: a queue cap prevents an unbounded pile-up of pending tracks, a race that could drop or duplicate queue items during high-traffic moments was fixed, stereo audio channels are now kept properly in sync, and the pre-playback buffering that smooths out the very start of a track was fixed to actually buffer the configured amount.

**The whole codebase went through two rounds of security and reliability review, and every issue found was fixed** — this release is the result, intended as a release candidate for live testing rather than a routine update.

## v0.4.0 — 2026-03-18

**Spotify sign-in moved to a proper login flow, and the bot now supports multiple people signing in with their own accounts.** `/login`, `/logout`, `/forget`, and `/who` let each person connect and disconnect their own Spotify account, and the bot can switch between whichever account is active. Sessions also now automatically reconnect and refresh their login if they drop, instead of just going silent.

**The now-playing display got a full visual upgrade.** It now shows rich embeds with album art and a link back to the track on Spotify, plus a lightweight history feed of recently played tracks, and the playback controls always stay pinned as the most recent message rather than getting buried under history.

**Added `/queue` to add a track by Spotify link without taking over playback**, and general UX polish across the board: a working pause/resume toggle, a join sound when the bot connects to voice, fixes for the "who's speaking" indicator and for an auto-logout bug, and buttons that no longer spam a visible reply every time they're pressed.

## v0.3.0 — 2026-03-17

**Voice playback was restored in regular Discord voice channels after Discord rolled out mandatory end-to-end voice encryption that broke it.** The bot had briefly needed a stage-channel workaround to keep working around that breakage; this release adds proper support for Discord's new encrypted-voice protocol directly, so the bot works in ordinary voice channels again with no workaround needed.

## v0.2.5-pre — 2026-02-09

**Added an interactive first-run setup wizard.** Running the bot with a `--setup` flag now walks through the configuration it needs step by step, instead of requiring manual setup of configuration files by hand.

## v0.2.0 — 2026-02-08

**First working release: the bot joins a Discord voice channel and streams whatever is playing on a linked Spotify Connect session into it.** This is the foundational audio pipeline — a Spotify Connect session feeding an internal audio buffer that gets piped into Discord voice.

**Connection reliability improved**, including an upgraded underlying Spotify connection library for more stable sessions and better handling of dropped connections so playback recovers instead of just stopping.

**Audio buffering and playback were tuned for smoother output**, reducing glitches in the streamed audio.

## v0.1.0 — 2026-02-02

**Project inception.** The repository was created with its license and an initial README; no bot functionality existed yet at this point — that arrived in the following release.
