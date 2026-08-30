# Spotibot -- full workspace audit #3
> Historical record from the v0.5 audit cycle, kept for provenance. The architecture it describes was superseded by the player-core refactor — see PORT.md.

**2026-07-16 | main @ a57f95d (v0.5.0-rc2) | working tree clean, main == origin/main**

Method: 12 auditor lenses (8 stock + stale/refactor, missing-features, port-readiness, workspace-hygiene), each report independently verified twice (Fable + Sonnet 5), then code-blind reconciliation. 48 agents, ~3.0M tokens, 1,069 tool calls. Only double-verified findings appear below.

**157 confirmed findings -- 0 critical / 15 high / 51 medium / 91 low.**

Ground truth established outside the fleet: `cargo test` = **53 passed, 0 failed**; tags stop at `v0.5.0-rc2` (no v0.5.0 final, local or origin); all four local side branches are fully merged into main.


---

## Stale code & refactor opportunities (34)

Vestigial code, duplication, dead surface, manifest/structure drift.

**[HIGH]** `src/discord/bot.rs:1843`
Priority-queue drain loop exists twice: priority_queue_manager (539-661) and the spawned task in trigger_priority_queue_drain (1843-1912) are ~70-line near-copies that have already diverged — the trigger copy posts the history embed unconditionally (1893) including for cancelled/skipped items while the manager breaks before posting on cancel (632-639); the manager pauses Spotify and clears the bridge per item (584-586) where the trigger copy does it once before the loop (1828-1837).

> **Fix:** Unify the two drain loops into one shared drain function; decide the correct history-post-on-cancel and pause/clear-bridge semantics once and apply to both entry points.

**[MEDIUM]** `Cargo.toml:42`
`futures-util = "0.3"` is a declared direct dependency with zero references anywhere in src/ (no `futures_util::`, no `use futures`). Unused direct dep — manifest hygiene drift; also misleads the nob port, since PORT.md treats this manifest as the reference dep list.

> **Fix:** Remove the `futures-util` entry from Cargo.toml.

**[MEDIUM]** `Cargo.toml:42`
futures-util is an unused direct dependency — zero occurrences of futures/futures_util anywhere in src/; it remains in the tree transitively via serenity/songbird.

> **Fix:** Delete the futures-util declaration from Cargo.toml.

**[MEDIUM]** `build.bat:3`
Still tracked despite the repo's own hardening plan ordering it untracked (docs/hardening-plan.md:51, item B5). Hardcodes a machine-specific path (C:\Program Files\Microsoft Visual Studio\2022\Community\...\vcvars64.bat) and has no .gitignore entry, so a plain `git rm --cached` would leave it reappearing as untracked. The other half of B5 (un-ignore .cargo/config.toml) was done; this half was not. Neither CLAUDE.md nor README.md references it as part of the build story.

> **Fix:** Complete B5's second half: `git rm --cached build.bat` AND add a `build.bat` entry to .gitignore so it doesn't reappear as untracked.

**[MEDIUM]** `src/discord/bot.rs:1794`
Voice-join logic duplicated with divergence: trigger_priority_queue_drain re-implements channel join + play_join_sound_then_bridge inline (1794-1826), parallel to join_voice_for_user (1305-1356), but omits the self-deafen step and stage-channel unsuppress handling (1330-1346).

> **Fix:** Replace the inline join with a call to join_voice_for_user (or a shared join helper) so deafen and stage-unsuppress apply on both paths.

**[MEDIUM]** `src/discord/bot.rs:1436`
Playback-teardown block (cancel feeder CancellationToken + clear priority_queue + clear active_priority_item) is copy-pasted four times: voice_state_update (990-1006), spawn_session (1436-1451), handle_stop (1968-1984), handle_logout (2242-2256).

> **Fix:** Extract a single stop_priority_playback() helper and call it from all four sites.

**[MEDIUM]** `src/discord/bot.rs:36`
80 copies of the std-Mutex poison-recovery incantation lock().unwrap_or_else(|e| e.into_inner()) in bot.rs (81 repo-wide). parking_lot is already a declared dependency (Cargo.toml:41) and already used for this reason in audio_bridge.rs.

> **Fix:** Switch bot.rs's shared-state Mutexes to parking_lot::Mutex (or a single lock-helper fn like UserStore::lock at users/mod.rs:87), deleting all 80 incantations.

**[MEDIUM]** `src/discord/bot.rs:431`
Spotify Web API responsibility is smeared across three modules, each building a fresh reqwest::Client per call with no timeout: spotify_playback_command (bot.rs:430-457), the inline queue POST in handle_queue (bot.rs:2314-2343, with hand-rolled percent-encoding via uri.replace(":", "%3A") despite pct_encode existing at oauth/mod.rs:235), and fetch_track_metadata (spotify/metadata.rs:41). SpotifyOAuth already holds a shared Client with a 10s timeout (oauth/mod.rs:77-81).

> **Fix:** Create a single Web API client module holding one shared, timeout-configured reqwest::Client; route all three call sites through it and reuse pct_encode (made pub) instead of the hand-rolled replace.

**[MEDIUM]** `src/discord/bot.rs:461`
Now-playing message choreography (delete previous now-playing, post history embed, delete old controls, send new card, store message id into both controls_message_id and now_playing_message_id) is implemented twice: post_priority_now_playing (461-507) for queue items and inline in run_presence_loop_with_track (744-793) for Spotify tracks, with small behavioral differences already present.

> **Fix:** Extract one shared now-playing-post helper used by both the priority-queue and Spotify presence paths.

**[MEDIUM]** `src/spotify/player.rs:86`
Discovery-era librespot credential Cache is vestigial: Cache::new(Some(dir), None, None, None) is a credentials-only cache; librespot persists reusable credentials into .spotify_cache but nothing ever reads them back — every (re)connect uses Credentials::with_access_token (line 238). MAX_CACHED_RECONNECTS (line 22) also carries stale 'cached' vocabulary. Only the device_id file (61-83) genuinely needs the directory.

> **Fix:** Drop the credentials Cache (keep only the device_id file handling for the directory) and rename MAX_CACHED_RECONNECTS to reflect that the loop reconnects with the OAuth token.

**[MEDIUM]** `src/spotify/player.rs:108`
Track metadata fetched twice per track over two protocols: fetch_track_info (librespot Track::get, 108-134, artists truncated to 2) fills the PresenceUpdate, then the presence handler re-fetches via Web API fetch_track_metadata (bot.rs:757-761, all artists + album art), yielding divergent artist formatting between bot status and embed. Caveat from verification: the librespot path also handles podcast episodes (player.rs:128-131), which /v1/tracks/{id} cannot serve.

> **Fix:** Deduplicate to a single metadata fetch per track feeding both the status text and the embed, preserving the librespot episode-handling path that the Web API cannot cover.

**[MEDIUM]** `src/users/mod.rs:242`
Vestigial legacy-JSON credential migration: LEGACY_CREDS_DIR (line 15) and import_legacy_json (242-285) import pre-SQLite .user_creds/*.json files on every open, but those tokens come from the removed pre-v0.5 client-secret flow which the PKCE-only refresh cannot use (bot.rs ~2122 documents this) — the migration can only import credentials guaranteed to fail their first refresh. The live VPS has already migrated (dir is deleted after import, line 282).

> **Fix:** Delete LEGACY_CREDS_DIR and import_legacy_json (and the open() call site); do not carry this block into the nob port.

**[MEDIUM]** `src/youtube/metadata.rs:26-28`
YoutubeError::DownloadFailed(String) is declared with a user-facing Display message ("Download failed: {0}") and marked #[allow(dead_code)], but is never constructed — the actual download path uses the separate FeederError::DownloadFailed (src/youtube/feeder.rs:17), which never converts to YoutubeError and never reaches the user. Both drain loops (bot.rs:640-643, 1883-1903) only tracing::warn (or nothing, in loop 2) on a non-cancel feeder error and post the history embed as if the track played. Downloads that fail after metadata succeeded are silent to the requester.

> **Fix:** Surface post-metadata download failures to the requester: bridge FeederError::DownloadFailed into the user-visible YoutubeError::DownloadFailed message (or an equivalent user-facing notification) in both drain loops, and stop posting the history embed as if the track played on a failed download. Alternatively, if silence is intended, delete the unconstructable variant.

**[LOW]** `.gitignore:18-20`
CLAUDE.md:95 claims `spotibot.db*` is gitignored, but .gitignore enumerates only the three exact names spotibot.db / spotibot.db-shm / spotibot.db-wal. A `spotibot.db-journal` (SQLite rollback-journal sidecar, produced if the DB ever runs outside WAL mode) would be committable. Narrow in practice (src/users/mod.rs:63 sets journal_mode=WAL), but the doc-vs-gitignore mismatch is real.

> **Fix:** Replace the three literal entries with the `spotibot.db*` glob CLAUDE.md already claims.

**[LOW]** `Cargo.toml:13`
`symphonia` is declared as a direct dependency but never referenced in code — it exists only as feature-plumbing (forcing the `pcm` codec via feature unification for songbird's RawAdapter path in src/discord/voice.rs:68). The manifest's convention is to comment non-obvious deps (sha2, chacha20poly1305, pbkdf2 all carry rationale comments); symphonia carries none, making it indistinguishable from an unused dep.

> **Fix:** Add a rationale comment to the symphonia line (matching the '(already compiled via ...)' convention on lines 27-29), or remove it if the pcm feature isn't actually load-bearing for the RawAdapter path.

**[LOW]** `Cargo.toml:42`
futures-util = "0.3" is a declared direct dependency with zero uses in src (no futures_util:: or futures:: anywhere; no build.rs). Dead scaffolding, likely left from the songbird-fork era; the crate is still pulled transitively by serenity/songbird.

> **Fix:** Remove the futures-util entry from [dependencies]; verify with cargo check (and optionally cargo machete / cargo +nightly udeps).

**[LOW]** `Cargo.toml:13`
The direct symphonia dependency looks unused (no import in src/) but is load-bearing: songbird 0.6.0 declares symphonia with default-features=false and no codec features, so this line's features=["pcm"] is what enables the pcm codec that decodes RawAdapter's f32 stream via feature unification. A future 'remove unused dep' cleanup would compile fine and break all audio at runtime.

> **Fix:** Add a comment on the symphonia line documenting that it exists to enable the pcm codec songbird needs for RawAdapter decoding.

**[LOW]** `Cargo.toml:47`
tokio-util features=["rt"] is unnecessary: the only tokio-util item used is tokio_util::sync::CancellationToken, and the sync module is unconditional in tokio-util 0.7 (rt gates only the context and task modules).

> **Fix:** Drop the features=["rt"] from the tokio-util declaration.

**[LOW]** `assets/icon.ico`
Orphaned tracked asset: referenced by nothing in the repo — no build.rs (so no winres/embed-resource icon embedding), no code reference, no mention in README.md, CLAUDE.md, or docs/. Added in an apparently unrelated commit (79a9c8a). A tracked file with no consumer is orphaned under the misplaced/orphaned convention.

> **Fix:** Either wire it up (add a build.rs icon embed) or drop the file from the repo.

**[LOW]** `src/discord/bot.rs:859`
check_ytdlp_available / check_ffmpeg_available (859-873) are generic external-binary probes with no Discord dependency, imported by main.rs at startup from discord::bot.

> **Fix:** Move both probes to src/youtube/ next to the code that shells out to those binaries.

**[LOW]** `src/discord/bot.rs:14`
Stale audit-marker comments from the v0.5 hardening burn-down: '// P0/P1 imports used below' (14), '// P1: Update button visual...' (1091), '// P1: Cancel any active YouTube/file feeder...' (1436), '// P0: Pause Spotify before feeding...' (1828). The specific P0/P1 IDs reference no enumerated list resolvable in the repo.

> **Fix:** Rewrite the four comments to describe the underlying reasoning instead of the ticket IDs.

**[LOW]** `src/discord/bot.rs:2284`
Leftover debugging logs at info level: handle_who logs 'attempting lock' / 'lock acquired' around a trivial mutex (2284-2286), and interaction_create logs 'interaction_create fired' (1026) plus per-interaction info logs on every button press (1030).

> **Fix:** Drop the deadlock-hunt relics and demote the per-interaction logs to debug/trace.

**[LOW]** `src/discord/bot.rs:1290`
user_can_play (1290-1303) re-implements the guild/voice-state lookup of user_in_bot_voice_channel (1269-1279); the two differ only in the None-bot-channel arm.

> **Fix:** Collapse into one function taking the bot-absent policy (or have user_can_play delegate to user_in_bot_voice_channel), removing the duplicate cache walk.

**[LOW]** `src/discord/voice.rs:9`
The 44.1kHz / 2-channel audio constants are declared independently in four modules with three different integer types: voice.rs:9-10 (u32), audio_bridge.rs:5-6 (usize), audio/dj.rs:16-17 (u32), youtube/feeder.rs:10-11 (u64) — plus librespot's SAMPLE_RATE/NUM_CHANNELS in sink.rs.

> **Fix:** Centralize the audio constants in one shared location (natural home: audio_bridge, which every producer/consumer already imports).

**[LOW]** `src/oauth/mod.rs:23-24`
TokenResponse.token_type is deserialized from Spotify's token responses but never read (marked #[allow(dead_code)]). Serde ignores unknown fields by default and TokenResponse has no deny_unknown_fields, so the field carries no wire-format necessity.

> **Fix:** Delete the token_type field and its #[allow(dead_code)]; deserialization is unaffected.

**[LOW]** `src/queue.rs:14-15`
MediaSource::YouTube.duration_secs is populated from yt-dlp metadata at bot.rs:1711 but never read anywhere afterwards (all later destructures use `..`); marked #[allow(dead_code)]. Capability gap: queued-track duration is invisible in every embed and the /queue-hint listing, and length is only enforced pre-queue in fetch_youtube_metadata.

> **Fix:** Either wire duration_secs into its expected consumers (duration display in now-playing/queue embeds, or in-playback length enforcement) or delete the field and its #[allow(dead_code)].

**[LOW]** `src/queue.rs:20-21`
MediaSource::File.content_type is populated from the Discord attachment at bot.rs:1730 but never read; marked #[allow(dead_code)]. feed_file_to_bridge instead derives the extension from the filename with a hardcoded "mp3" fallback (bot.rs:623, 1888), so MIME-based type detection for oddly-named uploads silently doesn't happen. Note: validate_attachment (src/youtube/metadata.rs:144-146) rejects extension-less filenames pre-queue, so the mp3 fallback is rarely reached — the dead field stands regardless.

> **Fix:** Either consult content_type in the feeder's format selection (fall back to filename extension when MIME is absent) or delete the field and its #[allow(dead_code)].

**[LOW]** `src/queue.rs:52-53`
QueueItem.queued_by_id is populated (bot.rs:1714, 1733) but never read in production — only the display-name string queued_by is used in embeds and DJ announcements; the id's only readers are queue.rs unit tests. Capability gap: nothing can identify the queuer by stable id; the global MAX_QUEUE_LEN=500 is the only flood control.

> **Fix:** Either add an id-based consumer (requester mention, requester-only skip, or per-user queue caps) or delete the field and its #[allow(dead_code)].

**[LOW]** `src/queue.rs:3`
YOUTUBE_MAX_DURATION_SECS is declared in queue.rs but consumed only by youtube/metadata.rs:104-107, and its doc comment describes env-override behavior implemented in that other module.

> **Fix:** Move the constant into src/youtube/ next to its only consumer.

**[LOW]** `src/queue.rs:14`
Dead data kept alive with #[allow(dead_code)]: MediaSource::YouTube.duration_secs (queue.rs:14-15), MediaSource::File.content_type (20-21), QueueItem.queued_by_id (52-53), TokenResponse.token_type (oauth/mod.rs:23-24, safe to delete since serde ignores unknown fields), and YoutubeError::DownloadFailed which is never constructed (youtube/metadata.rs:26-28; FeederError::DownloadFailed is the live one).

> **Fix:** Either wire these up (queued_by_id is the natural key for a per-user skip permission) or remove them along with their allow(dead_code) attributes.

**[LOW]** `src/spotify/metadata.rs:31-34`
SpotifyImage.width and .height are deserialized but never read (both #[allow(dead_code)]); fetch_track_metadata takes images.first() unconditionally (line 62). Capability gap: album art resolution is whatever Spotify lists first, never chosen. (In practice Spotify orders images largest-first, so first() yields the largest.)

> **Fix:** Either implement size-based album-art selection using width/height or delete both fields and their #[allow(dead_code)] and keep the first()-selection with a comment noting Spotify's largest-first ordering.

**[LOW]** `src/users/mod.rs:70-71`
spotify_credentials columns last_used_at and created_at are written on every save (mod.rs:155-162) but no query in the repo ever SELECTs or uses them — load/list read only the five other columns. PORT.md:98-99 documents the schema as intentional parity with nob's 002-music.sql, so this is write-only audit metadata in this repo.

> **Fix:** Either add a reader (stale-credential eviction, /who detail, or auto-start tie-breaking) or leave as-is with an explicit code comment pointing at the PORT.md parity rationale so the write-only columns aren't mistaken for dead schema later.

**[LOW]** `src/youtube/feeder.rs:193`
Real-time pacing deadline math is duplicated between DiscordSink::write (spotify/sink.rs:230-253, sync thread sleep+spin) and feed_pcm_to_bridge (feeder.rs:193-207, async sleep+yield, comment says 'mirrors DiscordSink'), with the same hard-coded 1-2ms tolerance constants in both places.

> **Fix:** Extract the frames-to-deadline arithmetic into one shared helper so the tolerance constants cannot drift; execution contexts can keep their own sleep strategies. Timed for the nob port when DSP moves to nob-audio per PORT.md.

**[LOW]** `src/youtube/metadata.rs:104-107`
Env var YOUTUBE_MAX_DURATION_SECS is parsed at runtime (overriding the 7200s const from src/queue.rs:4) but is documented nowhere an operator looks: absent from .env.example, from CLAUDE.md's env-var list, and from the setup wizard's .env template (src/setup.rs:312-357). A live operator knob that is invisible — effectively unconfigurable except by reading the source.

> **Fix:** Document YOUTUBE_MAX_DURATION_SECS in all three operator-facing surfaces: .env.example, CLAUDE.md's env-var list, and the setup wizard's .env template in src/setup.rs.


---

## Missing features & UX gaps (20)

Promised-but-absent behavior, dead-end flows, silent failures. Three entries are the known-deferred items from docs/audit2-followup.md, carried here so the list is complete.

**[MEDIUM]** `src/discord/bot.rs:1967-1998`
/stop never stops Spotify playback — it cancels the feeder, clears the priority queue/bridge, then unconditionally sends SpircCommand::Play (lines 1988-1995), resuming or continuing Spotify. README.md:41 and the command description promise stopping playback; with only a Spotify session active, /stop causes a brief bridge-clear hiccup then playback continues.

> **Fix:** In handle_stop, send SpircCommand::Pause (or an actual stop) to the Spotify session instead of unconditionally sending Play; only resume Spotify when the intent is to fall back from an interrupted priority item, matching the '/stop stops playback' promise in README.md:41 and the command description.

**[MEDIUM]** `src/discord/bot.rs:972-1021`
Empty-channel auto-leave is gated on an active Spotify session (has_session check at 973-978). YouTube/file-only playback started via /play with no /login leaves has_session=false, so when the voice channel empties the feeder keeps playing, the queue is not cleared, and the bot never leaves voice. README.md:28-29 promises auto-leave when the voice channel empties.

> **Fix:** Restructure voice_state_update so the feeder cancel, priority-queue clear, controls repost, and manager.leave() run whenever humans_in_bot_channel == 0, gating only the Spotify-session-specific teardown on has_session.

**[MEDIUM]** `src/discord/bot.rs:2000-2016`
/np ('Show what's currently playing') does not show the current track during Spotify playback — the Spotify branch (2011-2015) returns only 'Spotify session: {name}'. The presence loop already holds track metadata (last_meta in run_presence_loop_with_track) but it is not shared with handle_np. Priority items do show title/subtitle; Spotify, the primary source, does not.

> **Fix:** Promote the presence loop's last_meta to a shared Handler field (e.g., Arc<Mutex<Option<TrackMeta>>>) updated by run_presence_loop_with_track, and have handle_np read it to show title/artist for the Spotify branch.

**[MEDIUM]** `src/discord/bot.rs:640-646`
Feeder failures are invisible to users: in priority_queue_manager a non-Cancelled feeder error is only logged (640-642) and the history embed is still posted (646) as if the track played; in the /play-triggered drain the result is ignored entirely except Cancelled (1893-1903, history posted at 1893 before checking). A failed download shows a Now Playing embed, silence, then a history entry — no error feedback in the channel or to the requester.

> **Fix:** On non-Cancelled feeder errors in both drain paths, post a user-facing error message to the channel (or requester) and skip or annotate the history embed; also move the /play drain's post_priority_history after the result check and log non-Cancelled errors there.

**[MEDIUM]** `src/discord/bot.rs:1811-1823`
/play reports false success when the voice join fails: trigger_priority_queue_drain logs 'failed to join voice for standalone play' (1822) and continues; handle_play already replied '▶ Playing: **title**' (1764). With no call there is no bridge reader, so the item is consumed inaudibly with no feedback to the user.

> **Fix:** Propagate the join failure out of trigger_priority_queue_drain (return a Result) and have handle_play report the failure to the user instead of the unconditional '▶ Playing' reply; do not spawn the drain task when the join failed.

**[MEDIUM]** `src/discord/bot.rs:1056-1066`
ctrl_prev (⏮) is not priority-aware, unlike ctrl_next (1067-1085): while a YouTube/file item is playing it sends a Spotify 'previous' command to the paused baseline session (silently changing Spotify's position under the active YouTube track), or replies 'No active session' even though a track is audibly playing. No restart-current or 'not supported during queue playback' behavior.

> **Fix:** Mirror ctrl_next's structure: branch on priority_playing first in the ctrl_prev arm — either restart the current priority item or reply that previous isn't supported during queue playback — and only fall through to the Spotify 'previous' command when no priority item is active.

**[LOW]** `README.md:31-42`
README's slash-command table omits /announce (registered at src/discord/bot.rs:154-155, listed in CLAUDE.md); the 'What it does' bullet (line 27) mentions DJ announcements but never names the toggle command. A user reading README has no way to discover how to enable announcements.

> **Fix:** Add a /announce row to the README slash-command table ('Toggle DJ track announcements on/off') and name the command in the line-27 bullet.

**[LOW]** `README.md:89`
Stale test count in three docs: README.md:89, CLAUDE.md (Build and Run), and AGENTS.md:15 all say 'cargo test (48 unit tests)'; the tree has 53 #[test] functions across 10 files (none cfg-gated out).

> **Fix:** Update the count to 53 in README.md:89, CLAUDE.md:25, and AGENTS.md:15 — or drop the hard-coded number entirely so it can't go stale again.

**[LOW]** `README.md:69-71`
README's Configuration list omits the deployment-path env vars added post-v0.5.0 (YOUTUBE_TMP_DIR, YOUTUBE_COOKIES, DJ_CLIPS_DIR, DJ_CACHE_DIR, KOKORO_SOCKET — present in .env.example:29-35 and CLAUDE.md). README claims to enumerate optional tuning but stops at the pre-YouTube-merge set.

> **Fix:** Add YOUTUBE_TMP_DIR, YOUTUBE_COOKIES, DJ_CLIPS_DIR, DJ_CACHE_DIR, and KOKORO_SOCKET to README's optional configuration list, matching .env.example and CLAUDE.md.

**[LOW]** `docs/audit2-followup.md:69-72`
Known-deferred (accepted): CSRF state validation is skipped on a bare-code paste (security F4) — src/discord/bot.rs:2162-2168 validates state only when the pasted redirect carries one. Accepted because PKCE verifier binding mitigates and requiring state would break the paste-just-the-code UX.

> **Fix:** No code change — carry as an accepted-risk record in the fix-list. The deferral is documented at docs/audit2-followup.md (actual lines 68-71, one-line offset from cited range) and the mitigation (PKCE verifier binding) is confirmed in code.

**[LOW]** `docs/audit2-followup.md:73-76`
Known-deferred (operational call): yt-dlp is invoked with unpinned remote extractor components '--remote-components ejs:github' (security F11) at src/youtube/metadata.rs:60 and src/youtube/feeder.rs:43 — remote code fetched from GitHub at extraction time. Left as-is because pinning/removing it could break YouTube signature-challenge handling.

> **Fix:** Operator decision — either pin/remove the remote component (accepting possible YouTube extraction breakage) or keep the documented accepted-risk record at docs/audit2-followup.md (actual lines 72-75). No code change until the operator decides.

**[LOW]** `docs/audit2-followup.md:77-79`
Known-deferred (needs live audio to verify/tune): presence flap on Spotify auto-advance — the residual cosmetic half of edge F20; bot status briefly shows Idle between auto-advanced Spotify tracks (presence/reader timing around EndOfTrack, presence loop at src/discord/bot.rs:817-821).

> **Fix:** Defer until live audio testing on the VPS; then verify the flap reproduces and tune the presence/reader timing around EndOfTrack (Idle debounce or delay before flipping status). Statically unverifiable by design — documented at docs/audit2-followup.md (actual lines 76-78).

**[LOW]** `docs/components.md:11`
Docs promise ducking that was removed: the pipeline diagram (line 11, 'mixes on top with ducking') and the DJ section (line 57, 'mixer overlay with ducking') describe ducking machinery that audit2 burn-down deleted ('Dead ducking machinery removed, overlay mix preserved', docs/audit2-followup.md:32-33). No 'duck' identifier remains in src/.

> **Fix:** Remove the 'with ducking' phrasing from docs/components.md lines 11 and 57 so the docs describe the actual overlay mix without ducking.

**[LOW]** `src/discord/bot.rs:628-653`
Stale Now Playing card after a natural queue drain: the now-playing message is only deleted by the next post_priority_now_playing or by delete_and_repost_controls (logout/empty-channel/takeover). When the priority queue finishes naturally with no Spotify session to resume, the last item's 'Now Playing' embed (with live buttons) remains in the channel indefinitely alongside its history embed; buttons then answer 'No active session'.

> **Fix:** When either drain loop exits uncancelled with no Spotify session to resume (spirc_resume_tx is None), delete the now-playing message (or call delete_and_repost_controls) so no stale card with dead buttons persists.

**[LOW]** `src/discord/bot.rs:2406`
DJ announcements default off and the /announce toggle is not persisted — announce_enabled is a fresh AtomicBool(false) every boot, so every restart (including the VPS updater's restarts) silently disables announcements. Additionally /announce is voice-gated (needs_voice at line 1218), so it cannot be toggled before the bot is in a voice channel — dead-end when preconfiguring.

> **Fix:** Persist the announce toggle (SQLite settings row or env var read at startup) so restarts preserve it, and remove 'announce' from the needs_voice gate (or relax user_in_bot_voice_channel for it) so it can be set before the bot joins voice.

**[LOW]** `src/discord/bot.rs:160-165`
SoundCloud support is promised in README.md:26/40, CLAUDE.md, and AGENTS.md but invisible in the Discord surface: the /play command and option descriptions say 'YouTube URL' only, and all YoutubeError variants are YouTube-worded ('Couldn't find a video at that URL'). (The sub-claim that Discord truncates the option hint is unverifiable from the repo; the discoverability gap stands regardless since SoundCloud is never named.)

> **Fix:** Name SoundCloud in the /play command and url option descriptions (e.g., 'YouTube or SoundCloud URL') and generalize the YoutubeError message wording from 'video' to source-neutral phrasing.

**[LOW]** `src/discord/bot.rs:2288`
/who prints the raw numeric Discord ID ('Active session: **{spotify_name}** (Discord user {id})') even though ActiveSession carries discord_name; README promises 'Show the active DJ' — a snowflake ID is not a usable identity for channel members.

> **Fix:** Use session.discord_name (or a <@id> mention) in the /who reply instead of the raw snowflake.

**[LOW]** `src/setup.rs:329-333`
The --setup wizard never prompts for SPOTIFY_CLIENT_ID (writes it as a commented placeholder at 327-329), while main.rs:84-93 hard-errors without it — every first-run wizard completion dead-ends into a manual .env edit before the app can boot. README.md:59 documents the manual step, so this is a wizard-scope gap rather than undocumented behavior.

> **Fix:** Add a SPOTIFY_CLIENT_ID prompt to the wizard (and write the real value instead of the commented placeholder) so a completed --setup run produces a bootable .env. Note PORT.md drops the wizard for nob, so this fix is spotibot-only.

**[LOW]** `src/youtube/metadata.rs:104-107`
YOUTUBE_MAX_DURATION_SECS is read from env (overrides the 2h cap from src/queue.rs:4) but is documented nowhere — absent from .env.example, README, and CLAUDE.md's config lists. An undocumented knob on a user-facing limit ('Video too long (max N min)').

> **Fix:** Document YOUTUBE_MAX_DURATION_SECS (default 7200) in .env.example, README's config section, and CLAUDE.md's optional-vars list.

**[LOW]** `src/youtube/metadata.rs:138`
wma is accepted by validate_attachment (ALLOWED_EXTS line 138) but omitted from both user-facing lists: the InvalidFileType error message (line 29, 'Accepted: mp3, flac, ogg, wav, m4a, aac, opus') and the /play file option description (src/discord/bot.rs:168-169).

> **Fix:** Add wma to the InvalidFileType error message (metadata.rs:29) and the /play file option description (bot.rs:168-169) — or remove wma from ALLOWED_EXTS if it's not meant to be supported.


---

## Correctness bugs & edge cases (27)

Double-verified against the code. The two lenses overlap on the three worst issues (independent rediscovery -- treat as extra confidence, not double work).

**[HIGH]** `src/discord/bot.rs:584-592, 1829-1860, 699-706 (with src/spotify/player.rs:203-212)`
Priority (YouTube/file) items play into a paused output track: both drain paths send SpircCommand::Pause, sleep 300 ms, and only then set active_priority_item. The resulting PlayerEvent::Paused/Idle reaches run_presence_loop_with_track while active_priority_item is still None, so the !priority_active guard passes and pauses the shared bridge-reader TrackHandle. Nothing sends PresenceUpdate::Playing until Spotify resumes, so the songbird track stays paused for the whole priority item — audio never heard, bridge fills to its 8 s cap and drops, remnant cleared by sink.start() on resume.

> **Fix:** Set active_priority_item (or explicitly play the TrackHandle) BEFORE sending SpircCommand::Pause in both drain paths (priority_queue_manager and the /play-triggered drain), closing the 300 ms window in which the presence loop's guard sees no active priority item.

**[HIGH]** `src/discord/bot.rs:1067-1075`
The ctrl_next (⏭) button during priority playback cancels the feeder token and clears the bridge but, unlike handle_skip, never checks for more queued items, never re-triggers a drain, and never resumes Spotify. The owning drain breaks on FeederError::Cancelled and deliberately skips the resume ('skip/stop owns what plays next') — but the button handler owns nothing next either, so the bot stalls silent until another /play or /skip.

> **Fix:** Make the ctrl_next priority branch mirror handle_skip: after cancelling, check the queue snapshot and either re-trigger the priority-queue drain for the next item or send SpircCommand::Play to resume Spotify.

**[HIGH]** `src/discord/bot.rs:1067-1075`
The ⏭ button (ctrl_next) during priority (YouTube/file) playback only cancels the feeder token and clears the bridge — unlike /skip it never re-triggers the queue drain nor resumes Spotify, and the drain loop's Cancelled branch does neither. Remaining queued items are stranded and Spotify stays paused (dead air).

> **Fix:** Make ctrl_next mirror handle_skip: after cancelling, check has_more — if items remain, trigger_priority_queue_drain; otherwise send SpircCommand::Play to resume Spotify.

**[HIGH]** `src/spotify/player.rs:306, 331-337 (and src/discord/bot.rs:1595-1604)`
SpircCommand receiver permanently lost after first session drop: `spirc_cmd_rx.take()` moves the receiver into a per-iteration local inside the reconnect loop; on `continue` the local drops (closing the channel) and the Option stays None for all later iterations. The outer restart loop in bot.rs (spirc_rx.take() at 1603) is the same one-shot shape. All subsequent SpircCommand::Pause/Play sends from the priority-queue drains silently fail, so Spotify is never paused for priority playback and never resumed after it.

> **Fix:** Restructure receiver ownership so it survives reconnects and restarts — recreate the command channel per session iteration (or move the receiver back into the Option before `continue`) in run_with_token, and fix the matching one-shot shape in bot.rs's outer restart loop so re-spawned sessions get a live receiver.

**[HIGH]** `src/spotify/player.rs:306 (with src/discord/bot.rs:1595-1604, 584, 1834)`
SpircCommand receiver is consumed once via .take() on the first reconnect-loop iteration (and the bot-level restart loop passes None after the first call), so after any librespot session death+reconnect all Pause/Play sends silently fail — /play can't pause Spotify (interleaved garbled audio from both sources into one bridge) and queue-drain completion can't resume it. Only /login rebuilds the channel.

> **Fix:** Re-wire the SpircCommand receiver for every session iteration: create the channel (or a fresh receiver) inside the reconnect loop / per run_with_token call and update Handler.spirc_cmd_tx accordingly, instead of one-shot Option::take at both layers.

**[MEDIUM]** `src/discord/bot.rs:757-763 (token captured at src/spotify/player.rs:141, 184)`
The now-playing embed's metadata fetch uses the access token carried inside PresenceUpdate::Playing, captured once when run_with_token started and never updated by the proactive refresher (which only writes token_state, the DB, and ActiveSession). After ~1 h of a continuously healthy session, fetch_track_metadata 401s silently and every embed falls back to title/artist with no album art or canonical link — the exact stale-Web-API-token gotcha PORT.md documents.

> **Fix:** Read the fresh access token from active_session at metadata-fetch time (the presence loop already locks active_session in the same scope to read discord_name) instead of using the token embedded in the PresenceUpdate.

**[MEDIUM]** `src/discord/bot.rs:1931-1937`
handle_skip's next-item continuation is a fixed 200 ms sleep racing the DrainGuard release. The /play-triggered drain awaits a Discord HTTP call (post_priority_history at 1893) before dropping its guard, which can exceed 200 ms; trigger_priority_queue_drain's compare_exchange then fails silently. Remaining queue items never start and Spotify stays paused (no EndOfTrack arrives while paused, so the eot-driven manager never wakes).

> **Fix:** Replace the fixed 200 ms sleep with a deterministic handoff — e.g., retry trigger_priority_queue_drain until the guard is released, wait on the guard's actual release signal, or drop the DrainGuard before the post_priority_history HTTP call so cancellation releases it promptly.

**[MEDIUM]** `src/discord/bot.rs:1988-1995`
handle_stop unconditionally sends SpircCommand::Play whenever spirc_cmd_tx exists, even when no priority item was playing. A command documented as 'Stop playback and clear the priority queue' un-pauses Spotify, including sessions paused independently via the pause button.

> **Fix:** Gate the SpircCommand::Play resume in handle_stop on a priority item actually having been active (check active_priority_item before resuming), matching the guards handle_skip and the drain loops use.

**[MEDIUM]** `src/discord/bot.rs:1930-1937 (CAS at 1775, guard at 569/1845)`
/skip with more queued items sleeps a fixed 200 ms then calls trigger_priority_queue_drain; if the cancelled drain hasn't released drain_active yet (child.kill().await plus the history-embed HTTP post run before the guard drops), the compare_exchange fails silently — no new drain starts, Spotify stays paused, queue stalls with items in it.

> **Fix:** Replace the fixed 200 ms sleep with a deterministic handoff: retry the trigger until the CAS succeeds (or wait on drain_active release), and drop the DrainGuard before the post_priority_history HTTP call so the guard isn't held through network I/O.

**[MEDIUM]** `src/discord/bot.rs:1801-1810`
When /play needs to join voice, the code picks the first non-bot voice state in guild-wide HashMap iteration order (including other bots), not the requesting user's channel — the requester's id is never consulted despite the comment claiming 'Join the queuing user's voice channel'.

> **Fix:** Pass the requester's user id (already captured as queued_by_id in handle_play) into trigger_priority_queue_drain's join logic and join that user's voice channel via their voice state, falling back to the configured channel only if they're not in voice.

**[MEDIUM]** `src/discord/bot.rs:945-947, 962`
voice_state_update ignores the bot's own events and returns early when the bot has no cached voice state, so an admin force-disconnect leaves a zombie session: librespot, refresher, and the Spotify device stay alive with no voice connection, the bridge fills and drops indefinitely, and empty-channel auto-logout can never fire again.

> **Fix:** Detect loss of the bot's own voice connection — handle the bot's own voice_state_update disconnect (or register a songbird DriverDisconnect/CoreEvent handler) — and run full session teardown (abort librespot task, refresher, clear controls) when the bot is force-disconnected.

**[MEDIUM]** `src/spotify/player.rs:142, 184 (consumed at src/discord/bot.rs:757-758)`
Each Playing presence update embeds the access token captured once per run_with_token invocation; on a stable Connect session outliving ~1 h, fetch_track_metadata starts 401ing silently (warn only) and now-playing embeds degrade to no album art / librespot-only metadata. The refresher keeps ActiveSession.access_token fresh but the presence path never reads it (PORT.md documents this exact trap).

> **Fix:** Have the presence/metadata path read the current token from ActiveSession.access_token (kept fresh by the refresher) at fetch time, instead of embedding the once-captured token in every PresenceUpdate.

**[MEDIUM]** `src/spotify/sink.rs:233-248`
Pacing uses a cumulative deadline from a start_instant reset only on sink start/stop, never rebased after a decode stall. After a mid-track stall the target lags wall clock, writes burst unpaced, the bridge fills to AUDIO_BUFFER_SECONDS and drops samples (audible skip), and the backlog never drains — playback latency ratchets up permanently (button feedback heard ~8 s late at defaults).

> **Fix:** Rebase the pacing baseline after a stall: when the computed target falls behind wall clock (beyond a small threshold), reset start_instant/frames_sent so writes resume paced instead of bursting, preventing the one-way backlog ratchet.

**[MEDIUM]** `src/users/mod.rs:252-284`
import_legacy_json deletes the entire .user_creds directory (remove_dir_all) unconditionally, even when individual files were skipped (unreadable, malformed JSON, empty discord_user_id) or save() failed — those records are silently destroyed; all failure paths are bare `continue`s with no log.

> **Fix:** Track skips/failures and only remove the directory when every record imported successfully (or rename it as the doc comment says instead of deleting); log each skipped or failed record.

**[MEDIUM]** `src/youtube/feeder.rs:182`
feed_pcm_to_bridge aligns the carry buffer to 4-byte samples (`bytes.len() % 4`) instead of 8-byte stereo frames. When a pipe read contains an odd number of f32 samples (usable ≡ 4 mod 8), push_samples drops the trailing sample via `& !1`, shifting the interleaving by one and permanently swapping left/right channels for the rest of the track.

> **Fix:** Compute the carry modulo 8 (whole stereo frames) instead of modulo 4, so only complete L/R frames are ever pushed to the bridge.

**[MEDIUM]** `src/youtube/feeder.rs:174-176, 210-211`
feed_pcm_to_bridge treats ffmpeg stdout EOF as unconditional success: exit status is discarded (`let _ = child.wait().await; Ok(())`) and stderr is nulled, so corrupt/non-audio input exits non-zero with zero audio yet is logged 'finished' with a history embed and no user-facing error.

> **Fix:** Check the ffmpeg exit status from child.wait() after EOF; on non-zero exit (especially with zero samples fed), return a FeederError so the drain loop can report the failure to the user instead of posting a success history embed.

**[LOW]** `src/audio_bridge.rs:166-172`
push_overlay silently truncates any clip longer than the remaining bridge capacity (min 1 s at AUDIO_BUFFER_SECONDS=1) — no drop counter, no warn; a long Kokoro TTS announcement is cut mid-sentence.

> **Fix:** Count and warn on the truncated tail in push_overlay (mirroring total_dropped for the main buffer); consider feeding long overlay clips in chunks instead of one-shot truncation.

**[LOW]** `src/discord/bot.rs:1893 (vs. 632-646)`
The /play-triggered drain posts a history embed unconditionally before checking FeederError::Cancelled, so a skipped/stopped item still gets a 'played by' history card. The eot-driven priority_queue_manager breaks before posting history on cancel, and the handle_logout comment (2239-2241) confirms post-cancel history embeds are unwanted — the two drain paths disagree.

> **Fix:** Move the post_priority_history call in the /play-triggered drain to after the Cancelled check (skip it on cancellation), matching priority_queue_manager's behavior.

**[LOW]** `src/discord/bot.rs:1411-1418`
auto_start_stored_session passes user.spotify_username for both the spotify_name and discord_name parameters of spawn_session, ignoring the stored user.discord_name. After a boot auto-start, the controls card title and now-playing footers show the Spotify username where the Discord display name belongs.

> **Fix:** Pass user.discord_name as the third argument (discord_name) to spawn_session — the value is stored on UserCredentials and populated at login, just unused here.

**[LOW]** `src/discord/bot.rs:894-903 vs 933-935`
On first ready, startup_controls runs in a detached task while auto_start_stored_session → spawn_session → delete_and_repost_controls runs concurrently on the handler path. Both unconditionally overwrite controls_message_id (last-write-wins, no coordination); if startup_controls finishes second it clobbers the id with an idle 'Spotibot' card and orphans the active-user card, leaving two cards in the channel with later edits/deletes targeting the wrong one.

> **Fix:** Serialize the two controls posts on first ready — await startup_controls before auto_start_stored_session (or otherwise order/guard the controls_message_id writes) so the active-user card posted by spawn_session is authoritative and no orphan idle card survives.

**[LOW]** `src/discord/bot.rs:1988-1995`
handle_stop unconditionally sends SpircCommand::Play after clearing the queue, guarded only by the sender existing — running /stop while Spotify is paused or idle starts Spotify playback (a stop command that turns music on).

> **Fix:** Guard the SpircCommand::Play on prior playback state — only resume Spotify if priority playback was active and Spotify had been paused by the bot, not unconditionally.

**[LOW]** `src/discord/bot.rs:2367-2370`
prebuffer_samples can exceed the bridge's absolute capacity (AUDIO_BUFFER_SECONDS=1 → cap 88,200 vs default PREBUFFER_SECONDS=2.0 → target 176,400); the prebuffer loop in voice.rs can then never reach its target and always burns the full prebuffer_wait, silently adding fixed startup delay with a saturated, dropping buffer. No validation ties the two settings together.

> **Fix:** Clamp prebuffer_samples to the bridge's max capacity (or validate PREBUFFER_SECONDS <= AUDIO_BUFFER_SECONDS at config load, warning on adjustment) so the prebuffer loop can actually complete.

**[LOW]** `src/discord/bot.rs:889`
ready_tx is a capacity-1 channel whose receiver main() polls exactly once then parks forever without dropping; the second gateway ready fills the buffer and every ready after that blocks send().await forever, leaking one parked handler task per gateway resume beyond the second.

> **Fix:** Make the ready signal non-blocking — use try_send (ignoring Full/Closed) or drop ready_rx after the first recv so subsequent sends error immediately instead of parking tasks.

**[LOW]** `src/main.rs:86`
`&id[..8.min(id.len())]` byte-slices SPOTIFY_CLIENT_ID for the log prefix; a multi-byte UTF-8 character straddling byte index 8 makes the slice fall on a non-char boundary and panics at startup. Same pattern in setup.rs:181-182 for the token mask.

> **Fix:** Use char-boundary-safe truncation (e.g. iterate chars().take(8) or floor to a char boundary) for the client-id prefix and the setup.rs token mask instead of raw byte slicing.

**[LOW]** `src/oauth/mod.rs:225-229 (consumed at src/discord/bot.rs:2151-2154)`
parse_redirect's bare-code fallback accepts any 20-1024 char space-free string (including a redirect URL pasted without its query) as an authorization code with state=None, which also skips the CSRF-state comparison; the pending PKCE challenge is removed before the doomed exchange, so one bad paste burns the challenge and forces full re-authorization.

> **Fix:** Tighten the bare-code fallback (reject inputs that parse as URLs without a code param; constrain to a plausible authorization-code charset) and remove the pending PKCE challenge only after a successful token exchange so a corrected re-paste works without re-authorizing.

**[LOW]** `src/spotify/metadata.rs:41-48 (awaited inline at src/discord/bot.rs:757-758)`
fetch_track_metadata builds a reqwest Client with no timeout and is awaited inline inside the single presence loop; a hung Spotify API connection stalls all presence processing including handle.play()/handle.pause() mirroring. spotify_playback_command (bot.rs:430) and handle_queue (bot.rs:2315) share the no-timeout pattern but only block their own interaction.

> **Fix:** Add a timeout to these reqwest clients (mirroring the OAuth client's 10 s timeout); optionally move the metadata fetch off the presence loop's critical path so a slow fetch can't block play/pause mirroring.

**[LOW]** `src/youtube/feeder.rs:64-67, 90-93 (and src/audio/dj.rs:158-171)`
Unbounded disk growth: /skip or /stop during download kills yt-dlp after --no-part has begun writing and the partial yt-<uuid>.* file is never removed (cleanup only runs on success; `?` short-circuits past it on Cancelled); same for cancelled attachment writes. The tmp dir is never swept at startup, and the DJ cache accumulates one mp3 per unique announcement text with no eviction.

> **Fix:** Remove the partial file in the cancellation/error paths of download_youtube and download_attachment, sweep stale files from the tmp dir at startup (main.rs currently only creates it), and add eviction (TTL or size cap) to the DJ mp3 cache.


---

## Test coverage (16)

Suite is green (53/53) but the soak-critical paths are the untested ones.

**[HIGH]** `src/audio_bridge.rs:146-160, 166-179`
Coverage gap: the DJ overlay path is completely untested — push_overlay (even-frame guard, capacity bounding) and pull-side mixing (OVERLAY_GAIN, even-frame drain, mixing when music buffer is starved). Also unpinned: pull_samples' return value counts only music samples while overlay may be mixed into output beyond that count — voice.rs consumes the full buffer regardless, and nothing tests that pairing.

> **Fix:** Add tests for push_overlay (even-frame guard, capacity bound) and pull-side mixed output (OVERLAY_GAIN, starved-music mixing); pin the return-value-vs-mixed-output contract that voice.rs relies on.

**[HIGH]** `src/discord/bot.rs:1269-1305`
Coverage gap: user_in_bot_voice_channel / user_can_play — the authorization gate (PORT.md locked decision #4) — has zero tests. It guards every button, /queue, /play, /skip, /stop, /announce, and /login takeover, and is welded to the serenity cache so it can't be unit-tested as-is.

> **Fix:** Extract the channel-comparison logic into a pure function taking bot_ch/user_ch (a seam away from the serenity cache) and add tests pinning the gate behavior before the nob port rebuilds this dispatch.

**[HIGH]** `src/discord/bot.rs:2151-2168`
Coverage gap: complete_login's OAuth hardening paths are all untested — one-shot pending consumption (2153), 10-minute expiry rejection (2159), CSRF state-mismatch rejection (2163-2167). A pasted bare code (state: None) skips state validation entirely; no test documents whether that bypass is intended.

> **Fix:** Add tests for the one-shot pending consumption, the 600s expiry rejection, and the state-mismatch rejection; add a test pinning (or a decision on) the bare-code state-validation bypass.

**[HIGH]** `src/discord/voice.rs:73-150`
Coverage gap: SimpleBridgeReader::read has zero tests despite being unit-testable in-process. Untested: never returning Ok(0) (Songbird treats it as EOF, ending the track), the buf.len()<4 silence path (90-94), zero-filled output on starvation (113), f32→LE byte packing (136-141), prebuffer block/timeout (99-107). This is the soak-critical 'silence, not EOF' invariant.

> **Fix:** Add in-process tests using AudioBridge with prebuffer_wait=0 covering the never-Ok(0) invariant, the small-buffer silence path, starvation zero-fill, LE packing, and prebuffer blocking/timeout.

**[HIGH]** `src/youtube/feeder.rs:139, 178-188`
Coverage gap: the feeder module has zero tests. The carry-bytes logic (holding 1-3 trailing bytes when a pipe read misses an f32 boundary — dropping them 'would permanently shift every later sample and swap L/R') is untested, as is the pause rebase of `start` (147-157) preventing post-resume rushing/overflow.

> **Fix:** Extract the pure split/carry step and test it by feeding misaligned chunks and asserting sample continuity; add a test for the pause-rebase behavior.

**[MEDIUM]** `src/discord/bot.rs:539-661`
Coverage gap: priority_queue_manager — the soak-critical drain loop — is untested and untestable as written (14 params, serenity Context threaded through). The policy logic that matters: the drain_active compare-exchange single-flight guard (566) and cancelled-drain-must-not-resume-Spotify (632-638, 657-659).

> **Fix:** Extract the two policy decisions (single-flight guard, cancelled-drain-no-resume) into testable units and pin them in tests before the nob port rebuilds this manager against new seams.

**[MEDIUM]** `src/spotify/sink.rs:180-254`
Coverage gap: only the Biquad filter is tested; DiscordSink::write itself is not — preamp gain application, sample clamping to [-1,1], the DSP-off passthrough, and start/stop clearing the bridge and resetting pacing state.

> **Fix:** Add DiscordSink::write tests using AudioPacket::Samples with a small packet, covering preamp gain, clamping, DSP-off passthrough, and start/stop bridge-clear + pacing reset.

**[MEDIUM]** `src/users/crypto.rs:84-105`
Coverage gap: open()'s failure paths are partially untested — truncated blob (rest < NONCE_LEN), empty blob, unknown version byte all return None with no test. Tests run at KDF_ITERATIONS=1_000 under cfg(test), so no test ever executes the production derivation constants; PORT.md:102 still documents the retired 0x01 + sha256 scheme while code is 0x02 + PBKDF2.

> **Fix:** Add tests for truncated/empty/unknown-version blobs returning None, plus a golden-blob compatibility test (fixed key + fixed sealed blob must open) that pins the storage format so a KDF_SALT/iteration/scheme change can't silently orphan deployed rows.

**[MEDIUM]** `src/users/mod.rs:100-109, 242-285`
Coverage gap: store-level decrypt-failure handling is untested (the row_to_creds warn path that commit 619409b's recovery flow depends on), and import_legacy_json (242-285) is fully untested: JSON field extraction, skip-on-missing-id, delete-after-migration of plaintext token files.

> **Fix:** Add a test that saves under key A, reopens the DB under key B, and asserts load() returns None and list() skips the row; add import_legacy_json tests covering field extraction, skip-on-missing-id, and delete-after-migration.

**[MEDIUM]** `src/youtube/metadata.rs:136-150`
Coverage gap: validate_attachment is pure and completely untested — the 50MB cap, the extension allowlist, case-insensitivity, and the no-dot edge: a file literally named "mp3" yields ext="mp3" via rsplit('.').next() and is accepted; nothing pins whether that's intended.

> **Fix:** Add tests for the size cap, allowlist, and case-insensitivity, plus a test pinning (or a decision on) the no-dot filename acceptance.

**[MEDIUM]** `src/youtube/metadata.rs:73-111`
Coverage gap: fetch_youtube_metadata's classification branches are untested — stderr keyword routing to AgeRestricted/Unavailable vs the deliberately generic Network message (the 'don't leak cookie paths' rule, 83-87), live-stream rejection (99-101), and the YOUTUBE_MAX_DURATION_SECS env override (104-107).

> **Fix:** Extract the stderr classifier and the JSON→YoutubeMetadata mapping as pure functions and test the routing branches, the generic Network message, live-stream rejection, and the duration-cap override.

**[LOW]** `CLAUDE.md / PORT.md / README.md / AGENTS.md:CLAUDE.md:25, PORT.md:23, README.md:89, AGENTS.md:15`
Stale docs: the suite is 53 tests (both verifiers ran cargo test: 53 passed), not the 48 claimed in all four docs. PORT.md:102 also documents the blob scheme as 0x01 + sha256 key derivation while code is 0x02 + PBKDF2-HMAC-SHA256 — a porter following PORT.md verbatim would implement the wrong storage format.

> **Fix:** Update the test count to 53 in all four docs and correct PORT.md's blob-scheme documentation to 0x02 + PBKDF2-HMAC-SHA256 (the golden-blob test from the crypto finding is the mechanical backstop).

**[LOW]** `src/config.rs:24-37, 52-88`
Coverage gap: config tests only cover parse_id. env_num (distinguishing a typo'd value — warn + default — from unset) is untested, as are the clamp ranges (52-56) and the TEXT_CHANNEL_ID→DISCORD_CHANNEL_ID fallback on unset-or-invalid (85-88).

> **Fix:** Test env_num via a seam or serialized env-var tests, plus the clamp ranges and the TEXT_CHANNEL_ID fallback behavior.

**[LOW]** `src/discord/presence.rs:8-18`
Coverage gap: status_text is untested — the Idle/Paused/Playing mapping, the note-character flip, and the '{note} {title} - {artist}' format; only its truncate_status helper has tests.

> **Fix:** Add tests for the three-state mapping, the note flip, and the format string — pure function, one assertion each.

**[LOW]** `src/oauth/mod.rs:255-262`
Test quality: pkce_challenge_is_sha256_of_verifier computes its expected value with the same expression as the implementation — a mirrored oracle that passes even if both share the wrong encoding. It is the only soft-oracle test in the suite.

> **Fix:** Replace the mirrored oracle with the RFC 7636 Appendix B fixed vector (verifier dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk → challenge E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM), independently verified by both verifiers.

**[LOW]** `src/oauth/mod.rs:98-176`
Coverage gap: the three network methods (exchange_code, refresh_access_token, get_user_profile) have no tests, including non-2xx → OAuthError::Api paths and the display_name-fallback-to-id (175). URLs are hardcoded to Spotify hosts, so a mock server needs a seam. Token refresh is the single-owner lifecycle the session architecture leans on.

> **Fix:** Add a base-URL seam and test the three methods against a local mock server, covering the non-2xx error paths and the display_name fallback.


---

## Docs & comments drift (34)

Docs vs code, plus comment accuracy/convention.

**[MEDIUM]** `PORT.md:19`
Decision 6 claims token encryption key = sha256(TOKEN_ENC_KEY). The code stretches the key with PBKDF2-HMAC-SHA256 (600k iterations, fixed app salt) since commit 8a8a8d1 — src/users/crypto.rs:19-25,39-45. As the transfer dossier this would seed nob with the wrong scheme (mitigated only by the line 104 'port users/crypto.rs verbatim' instruction).

> **Fix:** Update decision 6 to state key = PBKDF2-HMAC-SHA256(TOKEN_ENC_KEY, fixed app salt, 600k iterations), matching src/users/crypto.rs.

**[MEDIUM]** `PORT.md:101-104`
Storage detail claims blob byte 0 = scheme '0x00 plaintext, 0x01 XChaCha20-Poly1305', layout [nonce||ciphertext]. Actual code: the encrypted version byte is 0x02 (V_XCHACHA_AAD); 0x01 appears nowhere. The ciphertext is additionally AAD-bound to the owner's discord_user_id, and plaintext rows are rejected when a key is set (downgrade protection) — none of which the dossier describes.

> **Fix:** Correct the version byte to 0x02 (V_XCHACHA_AAD) and document the AAD binding to discord_user_id plus the plaintext-row rejection (downgrade protection) when a key is set.

**[MEDIUM]** `README.md:31-42`
The 'Slash commands' table omits /announce (toggle DJ track announcements), which is a registered command and is voice-channel-gated. The command list at line 26 omits it too. The bot registers 10 commands; the table documents 9.

> **Fix:** Add an /announce row to the slash-commands table (voice-gated: yes) and add it to the line-26 command list.

**[MEDIUM]** `docs/components.md:11, 57`
Pipeline diagram says DJ TTS 'mixes on top with ducking' and the DJ section says 'mixer overlay with ducking'. Ducking was removed (audit #2 burn-down: 'Dead ducking machinery removed, overlay mix preserved'); the overlay now mixes at a fixed gain. No occurrence of 'duck' exists anywhere in src/.

> **Fix:** Remove the 'ducking' wording in both spots; describe the overlay as mixed on top at a fixed gain (OVERLAY_GAIN in src/audio_bridge.rs).

**[MEDIUM]** `src/discord/bot.rs:1800`
Outer comment says 'Join the queuing user's voice channel' but the code takes the first non-bot voice state in the guild (the accurate inner comment at 1803 says 'Find any human in a voice channel to follow') and falls back to the configured channel. The bot can join a different channel than the /play issuer's.

> **Fix:** Correct the outer comment to match the actual behavior: join the first human's voice channel found in the guild, falling back to the configured channel — not specifically the queuing user's.

**[MEDIUM]** `src/discord/voice.rs:143`
Comment claims the sleep paces the stream so Songbird cannot drain faster than real-time, but the sleep only fires when samples_read == 0 (starvation backoff). Real-time pacing lives in DiscordSink::write (src/spotify/sink.rs:230-231); PORT.md records 'don't move pacing to the reader.' The comment claims the wrong mechanism.

> **Fix:** Rewrite the comment to state the 10 ms sleep is a starvation backoff only, and that real-time pacing lives in DiscordSink::write.

**[MEDIUM]** `src/users/mod.rs:240-241`
Doc comment says the legacy directory is renamed; the code at line 282 calls std::fs::remove_dir_all — it deletes it. The inline comment at 280-281 directly contradicts the function's own doc comment. The doc drifted when rename was replaced by delete.

> **Fix:** Update the doc comment at 240-241 to say the legacy directory is deleted (matching remove_dir_all and the inline comment at 280-281).

**[LOW]** `AGENTS.md:45-46`
Dependency policy says 'sha2/chacha20poly1305 come free via songbird's DAVE — reuse, don't add crypto crates.' A third crypto crate, pbkdf2 0.12, has since been added as a direct dependency for TOKEN_ENC_KEY stretching (Cargo.toml:29). The policy line no longer describes the tree.

> **Fix:** Update the dependency-policy line to include pbkdf2 0.12 as a direct crypto dependency (transitive-made-direct for TOKEN_ENC_KEY stretching).

**[LOW]** `CLAUDE.md:38-42`
The 'Optional:' env list (and README's, and .env.example) omits YOUTUBE_MAX_DURATION_SECS, which the code reads to override the max queue-able video length. The variable is documented nowhere in the repo.

> **Fix:** Document YOUTUBE_MAX_DURATION_SECS (overrides the 7200s max queue-able video length, read in src/youtube/metadata.rs) in CLAUDE.md's Optional list, README.md's env list, and .env.example.

**[LOW]** `PORT.md:92-93`
Gotcha says the Kokoro socket client is 'cfg-gated to Linux'; docs/components.md:56 says 'Unix socket on Linux'. The code gates on #[cfg(unix)] (covers macOS/BSD too), with a non-unix stub. Socket path and env override are otherwise accurate.

> **Fix:** Change 'cfg-gated to Linux' to '#[cfg(unix)]' (Unix-family targets, with a non-unix stub) in PORT.md:92-93 and docs/components.md:56.

**[LOW]** `README.md:89`
Claims 'cargo test (48 unit tests)'. cargo test runs 53 tests (53 passed, 0 failed) — five tests were added after the docs were written (queue cap, crypto AAD/downgrade/KDF). The same stale '48' appears at CLAUDE.md:25, AGENTS.md:15, and PORT.md:23 (decision 9).

> **Fix:** Update '48 unit tests' to 53 in all four locations: README.md:89, CLAUDE.md:25, AGENTS.md:15, PORT.md:23.

**[LOW]** `README.md:69-71`
The 'Optional tuning' env list omits YOUTUBE_COOKIES, YOUTUBE_TMP_DIR, DJ_CLIPS_DIR, DJ_CACHE_DIR, and KOKORO_SOCKET — all read by the code and documented in .env.example and CLAUDE.md (added by commit d56e535 'config-ize deployment paths').

> **Fix:** Add the five missing env vars (YOUTUBE_COOKIES, YOUTUBE_TMP_DIR, DJ_CLIPS_DIR, DJ_CACHE_DIR, KOKORO_SOCKET) to the Optional tuning list, matching .env.example and CLAUDE.md.

**[LOW]** `README.md:44`
'Playback control (buttons, /queue, /play, /skip, /stop) requires sharing the bot's voice channel' — omits /announce (which IS gated) and overstates /play: when the bot is not yet in voice, /play only requires the user to be in any voice channel so the bot can follow them in (the fresh-boot exception audit2-followup documents as intentional). Same blanket claim at AGENTS.md:49-50, PORT.md:13-15 (decision 4), and docs/components.md:43.

> **Fix:** Add /announce to the gated list and note the /play fresh-boot exception (any voice channel suffices when the bot isn't in voice); apply the same correction at AGENTS.md:49-50, PORT.md:13-15, and docs/components.md:43.

**[LOW]** `docs/audit2-followup.md:36`
Heading says fixes landed 'post-v0.5.0 tag'. No v0.5.0 tag exists — tags stop at v0.4.0 then jump to v0.5.0-rc1/v0.5.0-rc2. The reference is to commit a57ac8b 'chore: release v0.5.0', after which the version was regressed to rc for live VPS testing.

> **Fix:** Reword the heading to reference commit a57ac8b ('chore: release v0.5.0') instead of a nonexistent v0.5.0 tag, noting the version was walked back to rc for live VPS testing.

**[LOW]** `docs/components.md:49`
Claims auth_blob key = 'sha256 of TOKEN_ENC_KEY'. Actual: PBKDF2-HMAC-SHA256, 600k iterations, fixed salt (same drift as PORT.md:19).

> **Fix:** Update to PBKDF2-HMAC-SHA256 (600k iterations, fixed salt), same correction as PORT.md:19.

**[LOW]** `docs/hardening-plan.md:19, 61`
Locked decision 8 and C5 promise a '401-retry fallback' / '401-retry-after-refresh wrapper around every Spotify Web API call'. No 401/UNAUTHORIZED handling exists anywhere in src/. The shipped architecture is proactive refresh + a Notify early-refresh signal — which is what PORT.md's decision 8 records instead; the two decision lists disagree.

> **Fix:** Annotate decision 8 and C5 as superseded: the shipped architecture is proactive refresh + Notify early-refresh signal (per PORT.md decision 8); the 401-retry wrapper was never implemented.

**[LOW]** `docs/hardening-plan.md:73`
D6 promises '/who gated by the same in-channel rule (security/F7)'. The code leaves /who ungated (needs_voice covers only queue/skip/stop/announce), and README's table documents /who as needing no voice channel — the plan item was dropped without a status note.

> **Fix:** Add a status note to D6 recording that the /who gating item was dropped (code and README both treat /who as ungated).

**[LOW]** `docs/hardening-plan.md:27`
Working rules are written in present tense — 'Work happens on branch v0.5-hardening; main stays untouched until a slice is validated.' The branch is fully merged; main is at v0.5.0-rc2 and is what the VPS deploys. The plan is executed through Phase H but carries no status marker, so its 'working rules' read as currently in force.

> **Fix:** Add a completion/status marker to the plan (executed through Phase H, branch merged to main) so the working rules read as historical, not in force.

**[LOW]** `src/:e.g. config.rs:39, audio_bridge.rs:72-73/107-108/149-150/168-169, queue.rs:69-70, discord/voice.rs:88-89, youtube/feeder.rs:47-48/82-84/136-139/144-146, discord/bot.rs:1209/1330/2219-2221, audio/dj.rs:282-283, oauth/mod.rs:202-203`
Convention (pervasive, low-grade): most substantive comments append a trailing rationale clause ('so the L/R interleaving never shifts', 'serenity's Id::new panics on 0', etc.). Each is attached to an accurate what-statement and most protected invariants are separately test-encoded, but under the strict what/how-only convention the why-clauses are non-conforming and are the natural rot points.

> **Fix:** Sweep the cited sites and trim the trailing rationale clauses to conform to the what/how-only convention, keeping the accurate what-statements; where an invariant is only protected by the prose, ensure a test covers it before trimming.

**[LOW]** `src/audio/dj.rs:270-271`
Comment says DJ announcements are unavailable on non-unix, but only TTS generation is unavailable: track_announce_clip falls back to a pre-recorded transition clip on kokoro failure, and join greetings decode via ffmpeg on any platform. The blanket 'unavailable' overstates.

> **Fix:** Reword the cfg(not(unix)) stub comment: only Kokoro TTS generation is unavailable on non-unix; clip-based announcements still play when DJ_CLIPS_DIR is populated.

**[LOW]** `src/audio_bridge.rs:13-14, 43`
Struct and method docs describe librespot/Spotify as the sole producer, but push_samples is also called by the YouTube/file feeder (src/youtube/feeder.rs:190) and the DJ overlay feeds a second deque. Drift from the YouTube merge.

> **Fix:** Update the struct doc and push_samples doc to name all producers: librespot/Spotify, the YouTube/file feeder, and the DJ overlay deque.

**[LOW]** `src/audio_bridge.rs:100-101`
Doc says the return value is 'the number of samples read', but it counts only the music-buffer drain; overlay samples are mixed into output afterwards, possibly beyond the returned count (music buffer empty + overlay non-empty returns 0 while writing audio). Doc predates the overlay-mixing step.

> **Fix:** Update the pull_samples doc to state the return value counts only the music-buffer drain and that overlay mixing may write samples into output beyond that count.

**[LOW]** `src/discord/bot.rs:1358-1360`
Doc says the function 'Skips when OAuth is unconfigured', but no such branch exists — self.oauth is a non-optional Arc<SpotifyOAuth> and main.rs exits at startup if SPOTIFY_CLIENT_ID is missing. Leftover from when oauth was optional (pre-v0.5 discovery removal).

> **Fix:** Remove the stale 'OAuth is unconfigured' skip-condition from the doc comment; keep only the real skip conditions (no active user, unparseable id / refresh failure).

**[LOW]** `src/discord/bot.rs:1762`
Comment says 'Nothing actively playing' but the guarding condition is only that no priority item is active (is_priority_playing == false); a Spotify session may be actively playing at this point — the triggered drain then pauses it.

> **Fix:** Reword the comment to state the actual condition: no priority item is active (Spotify Connect may still be playing and gets paused by the drain).

**[LOW]** `src/discord/bot.rs:1819`
Comment claims the code waits for 'join sound + bridge setup', but it is a fixed 500 ms sleep; play_join_sound_then_bridge connects the bridge reader only after clip duration + 0.1 s, and a DJ greeting clip can exceed 500 ms — the sleep does not guarantee what the comment claims.

> **Fix:** Reword the comment to state it is a fixed 500 ms grace delay, not a synchronized wait for the join sound and bridge connection.

**[LOW]** `src/discord/bot.rs:14, 1091, 1436, 1828`
Convention (iteration narration/history): 'P0'/'P1' are audit burn-down ticket labels, meaningless to a reader of the current code. Line 14 in particular annotates imports with audit-priority history.

> **Fix:** Strip the P0/P1 tags at all four sites; the trailing what-statements stand alone. Delete the line-14 comment entirely.

**[LOW]** `src/discord/bot.rs:1391-1394, 2122-2126`
Convention (history): both comments explain the failure by reference to the pre-v0.5 client-secret flow, which no longer exists in the codebase. The version-archaeology clause is history that will rot.

> **Fix:** Remove the pre-v0.5 client-secret flow clauses from both comments; keep the what ('dead refresh token -> deactivate, issue fresh authorize URL').

**[LOW]** `src/discord/bot.rs:536-537`
Convention: the second sentence is port-roadmap narration about a different repo's future structure. The mapping already lives in PORT.md ('bot.rs -> split across commands/actions/panel/player'); duplicating it in a comment invites drift.

> **Fix:** Delete the nob-port sentence from the comment; keep the first sentence describing what the function wires together. PORT.md already carries the mapping.

**[LOW]** `src/main.rs:82-83`
Convention (history): 'since discovery/mDNS was removed in v0.5' is version history, not what the code does; the removal history already lives in PORT.md (decision 7).

> **Fix:** Drop the version-history clause; keep 'OAuth is the only session path; PKCE needs the client id only'.

**[LOW]** `src/oauth/mod.rs:96-97`
Convention (rationale/editorial): 'that is the whole point of PKCE' is pure justification; 'No client secret is sent' already states the what.

> **Fix:** Delete the trailing 'that is the whole point of PKCE' clause; keep 'No client secret is sent'.

**[LOW]** `src/spotify/player.rs:25`
Doc says SpircCommand is sent from the priority queue manager, but it is also sent from handle_skip (bot.rs:1945), handle_stop (bot.rs:1994), and the /play-triggered drain task (bot.rs:1834, 1909). The doc understates the senders.

> **Fix:** Update the doc to list all SpircCommand senders (priority queue manager, skip/stop handlers, the /play-triggered drain task) or generalize to 'sent from the Discord command layer'.

**[LOW]** `src/users/crypto.rs:16-18, 20-21`
Convention (rationale): 'a fixed salt is fine here' and 'OWASP-recommended' are security-decision justifications, not what/how. 'The derivation logic is identical' under test is an invariant asserted only in prose — nothing encodes that the cfg(test) path differs solely by iteration count.

> **Fix:** Trim the rationale clauses to what/how per the convention; if the test-vs-prod KDF invariant matters, encode it (test or structural assertion) rather than assert it in prose.

**[LOW]** `src/users/mod.rs:280-281`
Convention (iteration narration/why): 'rather than renaming it' contrasts with the abandoned previous implementation, and the rest is rationale rather than what/how.

> **Fix:** Reword to a what-statement such as 'Delete the legacy dir so no plaintext token files remain'; coordinate with the finding-0 fix to the doc comment at 240-241.

**[LOW]** `src/youtube/metadata.rs:113-116`
Convention: a rationale-only comment for deliberately absent code — no what beneath it. The load-bearing invariant ('metadata success implies the video is playable with current cookie config') exists only as prose; nothing enforces it, so a future defensive age_limit check would silently reintroduce the bug this omission fixed.

> **Fix:** Rewrite the comment as a what-statement tied to the surrounding code, and encode the invariant mechanically (a test asserting no age_limit gating on metadata success) so the protection isn't prose-only.


---

## NOB port readiness (12)

PORT.md (the transfer dossier) verified against current code.

**[HIGH]** `PORT.md:70-71`
The Spirc-lifecycle gotcha prescribes the design the code has since abandoned: "The spirc task must be tokio::spawn'ed independently so a command-listener loop can't drop it." Current code deliberately does the opposite — src/spotify/player.rs:305-322 pins spirc_task inline in a select! loop so its completion (session death) breaks out to the reconnect path and cancellation propagates; the event loop is guarded by AbortOnDrop (player.rs:37-42, 270) instead of detached. A porter following the dossier re-introduces the bug the repo already fixed.

> **Fix:** Rewrite the Spirc-lifecycle gotcha to describe the current design: pin spirc_task inline (tokio::pin! inside a select! against the command receiver) so session death breaks to the reconnect path and cancellation propagates, and guard the event loop with AbortOnDrop. Remove the detached tokio::spawn prescription — note it was the old design that parked forever on cmd_rx.recv().

**[HIGH]** `PORT.md:19`
Decision 6 says the token-encryption key is "key = sha256(TOKEN_ENC_KEY)". Code stretches it with PBKDF2-HMAC-SHA256, 600,000 iterations, fixed salt "discord-spotify-player:token-enc:v1" (src/users/crypto.rs:19-25, 39-44; commit 8a8a8d1, after PORT.md was written). A nob port that derives sha256(key) produces a different cipher key entirely.

> **Fix:** Update Decision 6 to state the actual derivation: key = PBKDF2-HMAC-SHA256(TOKEN_ENC_KEY, salt="discord-spotify-player:token-enc:v1", 600,000 iterations), replacing the sha256(TOKEN_ENC_KEY) claim.

**[HIGH]** `PORT.md:101-104`
The auth_blob format description is stale: it says byte 0 = scheme "0x01 XChaCha20-Poly1305" then [nonce||ciphertext]. Code uses version byte 0x02 (V_XCHACHA_AAD) with the ciphertext bound to the row owner via AEAD associated data (aad = discord_user_id, src/users/crypto.rs:13-14, 56-77), rejects plaintext rows when a key is configured (anti-downgrade, crypto.rs:86-89), and has no 0x01 handler at all. The AEAD owner-binding hardening (commit d56e535) is entirely absent from the dossier.

> **Fix:** Rewrite the auth_blob format description: version byte 0x02 (V_XCHACHA_AAD) with [nonce(24) || ciphertext] and AAD = discord_user_id (owner-binding), plus the anti-downgrade rule that plaintext (0x00) rows are rejected when a key is configured. Remove the 0x01 scheme — the code has no handler for it.

**[MEDIUM]** `PORT.md:72-79`
The unrefreshable-token recovery path (rc2 commit 619409b) is missing from the token-refresh/OAuth sections: auto-start failure now deactivates the stored row and posts a /login prompt to the text channel instead of silently retrying every boot (src/discord/bot.rs:1391-1406), and reactivation failure deactivates and falls through to a fresh PKCE authorize URL via issue_login_url instead of dead-ending the user into /forget + /login (src/discord/bot.rs:2062-2068, 2117-2131). Paid for in blood on the live VPS (v0.4 client-secret-minted refresh tokens are unrefreshable under PKCE) and nob inherits the same token classes.

> **Fix:** Add the unrefreshable-token recovery path to the token-refresh/OAuth gotchas: on auto-start refresh failure, deactivate the stored row and post a /login prompt to the text channel; on reactivation failure, deactivate and fall through to a fresh PKCE authorize URL (issue_login_url) instead of dead-ending into /forget + /login. Note the token classes that trigger it (revoked, pre-PKCE client-secret-minted).

**[MEDIUM]** `PORT.md:86-90`
The EndOfTrack tail-trim lesson (commit 985db75) is missing from the paid-for gotchas: the bridge must NOT be cleared on a natural track boundary or the tail of an auto-advancing track is trimmed — PlayerEvent::Stopped handles real stops, and a priority-item drain clears the bridge itself before playing (src/spotify/player.rs:203-208). The dossier covers pacing and frame parity but not this bridge-lifecycle rule, while telling nob to rebuild the manager against its own seams.

> **Fix:** Add a bridge-lifecycle gotcha: never clear the AudioBridge on PlayerEvent::EndOfTrack (natural track boundary) or the tail of an auto-advancing track gets trimmed; real stops are handled by PlayerEvent::Stopped, and a priority-item drain clears the bridge itself before playing.

**[MEDIUM]** `PORT.md:52-56`
The drain-serialization lesson (commit 3aa49ef) is missing: queue drains are single-owner via an AtomicBool compare_exchange with an abort-safe DrainGuard that clears the flag on drop so a cancelled/panicking drain can't wedge all future drains (src/discord/bot.rs:108-110, 526-533, 564-569, 1772-1778, 1844-1845). The dossier tells nob to rebuild the priority-queue manager rather than paste bot.rs, so the /play-triggered-drain vs eot-manager race and its abort-safety fix need recording or nob re-hits both.

> **Fix:** Record the drain-serialization lesson alongside the rebuild-the-manager instruction: queue drains must be single-owner (AtomicBool compare_exchange) with an abort-safe guard that clears the flag on drop, covering the /play-triggered-drain vs end-of-track-manager race and ensuring a cancelled or panicking drain can't wedge future drains.

**[MEDIUM]** `PORT.md:47`
The config-row instruction "Merge the Spotify/token keys into nob's config struct" undercounts the config surface: the deployment-path keys added in d56e535/3971484 are module-local env reads that never pass through src/config.rs — YOUTUBE_COOKIES (default /var/lib/spotibot/youtube-cookies.txt) and YOUTUBE_TMP_DIR (default /tmp/spotibot-youtube) in src/youtube/mod.rs:4-15, DJ_CLIPS_DIR / DJ_CACHE_DIR in src/audio/dj.rs:6-7, KOKORO_SOCKET in src/audio/dj.rs:262-267. A port-time config unification driven by the dossier misses all five.

> **Fix:** Expand the config row to enumerate the five module-local env keys that bypass src/config.rs — YOUTUBE_COOKIES, YOUTUBE_TMP_DIR, DJ_CLIPS_DIR, DJ_CACHE_DIR, KOKORO_SOCKET — with their /var/lib/spotibot and /tmp defaults, so a port-time config unification captures all of them.

**[MEDIUM]** `PORT.md:46`
The youtube row ("yt-dlp feeder + metadata") predates the rc2 cookies/age-gate behavior (commit 3971484): the cookies file is passed to yt-dlp only if it exists on disk (src/youtube/metadata.rs:59-62, src/youtube/feeder.rs:42-45), the metadata age_limit>=18 reject was deliberately removed because reaching metadata means cookies already unlocked the video (metadata.rs:113-116), and the no-cookie age-gate case is classified from stderr into an actionable user error (metadata.rs:18). None of this behavior contract is in the dossier.

> **Fix:** Document the cookies/age-gate behavior contract in the youtube row: --cookies is passed to yt-dlp only when the file exists on disk (both metadata and feeder paths); no age_limit reject after metadata succeeds (reaching metadata means cookies unlocked the video); the no-cookie age-gate failure is classified from stderr into an actionable AgeRestricted error pointing the admin at YOUTUBE_COOKIES.

**[LOW]** `PORT.md:45`
The queue row omits the queue cap shipped after the dossier was written (commit 047c24d): MAX_QUEUE_LEN = 500 and push() now returns bool, rejecting past the cap (src/queue.rs:56-79, tested at 133-143). The code comment says the value matches nob's unified-queue cap, so impact is limited, but the changed push signature and rejection semantics are part of the transplant contract.

> **Fix:** Note the queue cap in the queue row: MAX_QUEUE_LEN = 500 (matching nob's unified-queue cap) and the fallible push() -> bool signature that rejects at capacity.

**[LOW]** `PORT.md:32-48`
The module map has no row for src/presence.rs — the 11-line shared PresenceUpdate enum (Idle/Paused/Playing carrying title, artist, track_id, access_token) that both the player-event side and src/discord/presence.rs depend on. It needs an assigned home in nob; the map only covers src/discord/presence.rs.

> **Fix:** Add a module-map row for src/presence.rs (the shared PresenceUpdate enum) with an assigned home in nob — either the presence module or a shared-types module — since both the player-event side and src/discord/presence.rs depend on it.

**[LOW]** `PORT.md:23`
Decision 9 says "48 unit tests"; the tree now has 53 #[test] functions (queue-cap and crypto AAD tests added after the dossier was written). Same stale count appears in CLAUDE.md. Trivial drift, but it is a locked-decision claim used as the port's quality bar.

> **Fix:** Update the test count from 48 to 53 in PORT.md Decision 9 and in CLAUDE.md (or drop the literal count in favor of a non-brittle phrasing).

**[LOW]** `PORT.md:52`
"It is a 2200-line god-module" — src/discord/bot.rs is now 2490 lines after the post-dossier fixes (recovery path, drain guard, etc.). Cosmetic staleness, but it signals the do-not-port-wholesale section hasn't been re-read against the ~290 lines of new bot.rs logic, some of which carries port-relevant lessons.

> **Fix:** Update the line count (bot.rs is now ~2490 lines) and re-read the do-not-port-wholesale section against the ~290 lines of post-dossier bot.rs logic to confirm the port-relevant lessons (drain guard, recovery path) are captured by the other fixes.


---

## Workspace & agent-config hygiene (11)

Branches, local artifacts, Claude settings/memory.

**[HIGH]** `.claude/settings.local.json:44 (also 6, 19)`
Overly-broad permission allow rules: "Bash(powershell.exe *)" (line 44) permits arbitrary command execution, and "Bash(powershell -Command:*)" (line 6) / "Bash(powershell -c:*)" (line 19) are equivalent escape hatches — any command can be routed through PowerShell, bypassing the permission system entirely.

> **Fix:** Delete all three entries and re-allow only the specific PowerShell invocations actually needed.

**[MEDIUM]** `.claude/settings.local.json:31 (also 24, 37, 43)`
"Bash(git push:*)" (line 31) and "Bash(git tag:*)" (line 43) auto-approve outward-facing release actions, conflicting with the user rule that pushes/releases need explicit go-ahead per action; "Bash(git rebase:*)" (37) and "Bash(git checkout:*)" (24) auto-approve history-rewriting/tree-switching ops. The push rule is production-relevant: the VPS spotibot updater polls main every 5 minutes, so an unprompted push deploys.

> **Fix:** Remove git push and git tag from the allowlist; keep checkout/rebase only if the prompt friction is deliberate.

**[MEDIUM]** `C:\Users\zahac\.claude\projects\C--Users-zahac-Desktop-CS-Projects-discord-spotify-player\memory\MEMORY.md:4`
Index line for v0.5 hardening says "H (audit#2) + release (needs go) + nob port left" — stale. Phase H is done, v0.5.0-rc1 and v0.5.0-rc2 are tagged and pushed to origin (rc2 = a57f95d = current main HEAD), released 2026-07-10 with Achille's go. Only the nob port remains.

> **Fix:** Rewrite the index line to "released v0.5.0-rc2 (live on VPS), nob port left".

**[MEDIUM]** `C:\Users\zahac\.claude\projects\C--Users-zahac-Desktop-CS-Projects-discord-spotify-player\memory\spotibot-v05-hardening.md:22`
Stale/contradictory paragraph: says "After tagging v0.5.0 locally" (no v0.5.0 tag exists — only v0.5.0-rc1/rc2) and frames pushing v0.5.0 vs folding fix/audit2-followup as an open decision ("his call") — superseded by line 16's own record that fix/audit2-followup was folded into main and released as rc1 (then rc2). Test counts in the file ("48 unit tests" line 10, "52 tests total" line 22) have drifted: cargo test on main runs 53.

> **Fix:** Rewrite line 22 to past tense (folded in, shipped in rc1/rc2) and correct the test count.

**[LOW]** `(git repo — local branches)`
Four stale local branches, all confirmed merged into main via git branch --merged main: feat/now-playing-channel and feat/youtube-support (both frozen at afd88f0 = v0.2.0-era), v0.5-hardening (a57ac8b), and fix/audit2-followup (daabaca). The latter two are local-only (no origin tracking refs).

> **Fix:** git branch -d feat/now-playing-channel feat/youtube-support v0.5-hardening fix/audit2-followup (safe — -d refuses unmerged work).

**[LOW]** `(git repo — remote branches)`
Four stale remote branches, all confirmed merged into main via git branch -r --merged main: origin/feat/now-playing-channel and origin/feat/youtube-support (v0.2.0-era, afd88f0), origin/feat/oauth-login (690fd79) and origin/feat/youtube-playback (e847d6f) — the latter two were folded into v0.5-hardening and shipped in rc1/rc2.

> **Fix:** git push origin --delete feat/now-playing-channel feat/youtube-support feat/oauth-login feat/youtube-playback — outward-facing deletion, needs Achille's explicit go before executing.

**[LOW]** `.claude/settings.local.json:7-8 (also 41-42)`
Lines 7-8 hardcode the repo's old location c:\Users\zahac\Desktop\CS\Code\apps\discord-spotify-player (cargo check via vcvars64, and build.bat) — that path no longer exists on disk (repo now lives under Desktop\CS\Projects\), so these entries can never match a useful command again. Lines 41-42 are stale one-off commands (Desktop .lnk shortcut inspection, Pillow availability check).

> **Fix:** Delete all four entries.

**[LOW]** `.env`
Present at repo root (987 B, untouched since Mar 17) — contents not read per audit scope. Properly gitignored (.gitignore line 7); the ignored-coverage sweep found zero untracked files at root, so .gitignore fully covers everything present (.claude/, .env, .spotify_cache/, releases/, target/).

> **Fix:** No action required — informational finding noting existence and confirming gitignore coverage.

**[LOW]** `.spotify_cache/credentials.json`
Legacy cached Spotify credential file (282 B, last written Jun 10 — predates the v0.5 OAuth-only rework) in .spotify_cache/. The directory itself is still live (src/spotify/player.rs:20 sets CACHE_DIR = ".spotify_cache"), but credentials.json is credential material at rest from the pre-hardening era; current auth stores tokens encrypted in spotibot.db. Gitignored, so no leak risk via git.

> **Fix:** Delete credentials.json (librespot regenerates cache files it actually needs); keep the directory.

**[LOW]** `CLAUDE.md:25`
Stale test count "48 unit tests" — cargo test on main currently runs 53 (verified by both runs: "53 passed; 0 failed"). Same stale count in AGENTS.md:15, README.md:89, and PORT.md:23 (decision #9).

> **Fix:** Update all four occurrences to 53 or drop the hardcoded number (it drifts every time tests are added).

**[LOW]** `releases/`
Stale local release artifacts: discord-spotify-player-0.1.0-win64.zip (8.1 MB, Feb 3) and discord-spotify-player-0.2.5-pre-win64.zip (10.5 MB, Feb 9) plus README.txt. Gitignored (releases/ and *.zip both covered), superseded by git tags v0.1.0/v0.2.5-pre and five releases since; nothing in the repo references the directory.

> **Fix:** Delete the directory (~18.6 MB reclaimed); tags preserve the history.


---

## Security (placed last, as requested) (3)

Attacker model: someone sharing the voice channel (jukebox-among-friends), but the bot co-lives on the VPS with Sidearm.

**[MEDIUM]** `src/youtube/metadata.rs:58-71 (fetch_youtube_metadata); also src/youtube/feeder.rs:37-55 (download_youtube), reached from src/discord/bot.rs:1703`
Server-side request forgery (SSRF) via the /play URL. The attacker-supplied string is passed straight to yt-dlp's generic extractor, which issues a server-side HTTP GET to any host/scheme given — no allowlist of hosts (youtube/soundcloud) or scheme restriction. An in-voice attacker can probe the co-located Sidearm loopback panel (127.0.0.1:18789), cloud-metadata endpoints (169.254.169.254), or other internal services; fetched page <title>/OpenGraph metadata is reflected back into the Now Playing embed and TTS announcement, giving partial response reflection and internal-service/port probing. Not covered by any accepted risk in docs/audit2-followup.md.

> **Fix:** Add a host/scheme allowlist (e.g., https-only, youtube/soundcloud domains) validated before invoking yt-dlp in both fetch_youtube_metadata and download_youtube, so arbitrary URLs never reach the generic extractor.

**[LOW]** `src/discord/bot.rs:1703 (fetch_youtube_metadata call in handle_play)`
Unbounded external-process spawning from /play metadata resolution. Each /play url: invocation spawns a yt-dlp --dump-json subprocess (network fetch + JSON parse) with no per-user rate limit and no in-flight concurrency cap. The spawn runs before the queue push, so the 500-item queue cap and single-drain serialization do not throttle it. Rapid /play invocations (one or several colluding in-voice users) can spawn many concurrent yt-dlp processes, driving CPU/FD/PID pressure on the VPS that also hosts the Sidearm agent runtime.

> **Fix:** Gate the metadata probe before the subprocess spawn: add an in-flight concurrency cap (e.g., a semaphore around yt-dlp spawns) and/or a per-user rate limit/cooldown on /play, enforced ahead of fetch_youtube_metadata.

**[LOW]** `src/presence.rs:1-11`
PresenceUpdate uses #[derive(Debug)] while its Playing variant holds access_token: String (the live Spotify OAuth access token). Any future or accidental tracing::debug!(?update) / {:?} on a PresenceUpdate would write the bearer token into logs. The value is cloned and forwarded through channels (bot.rs:689). Latent, not currently triggered. UserCredentials (users/mod.rs:29-40) already has a manual redacting Debug for exactly this pattern; PresenceUpdate does not.

> **Fix:** Replace the derived Debug on PresenceUpdate with a manual Debug impl that redacts/skips the access_token field, mirroring the existing UserCredentials treatment.
