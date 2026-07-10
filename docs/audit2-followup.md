# Audit #2 — follow-up (deferred findings)

Audit #2 (2026-07-10, 8 lenses, double-verified) on the v0.5-hardening tree
returned **128 confirmed findings** (4 critical, 19 high, 46 medium, 59 low).
The merged YouTube branch (+1800 lines, never previously audited) was the main
source.

## Fixed (committed on this branch)

- **All 4 criticals**: `run_with_token` blocking forever on `cmd_rx.recv()`
  (session death never observed); detached spirc/event-loop tasks surviving
  logout as ghost audio; the shared bridge-reader track paused by Spotify
  idle/pause while a YouTube feeder was active.
- **Session-lifecycle highs**: give-up path now aborts the refresher instead of
  detaching it; the bot's own `voice_state_update` is ignored (was re-breaking
  the auto-start P0); auto-start skips on a failed boot refresh instead of
  spawning with a stale token.
- **Real correctness bugs**: yt-dlp argv injection (`--` sentinel);
  `error_for_status()` on the download; the YouTube f32 stream's dropped-tail
  desync (carry buffer); overlay mix stereo-frame parity; `/skip` surfacing
  API failure; credential decrypt-failure warning; removed hardcoded
  `/opt/openclaw` debug file writes.

## Also fixed in a second burn-down pass

- Spurious `SpircCommand::Play` after a skip/stop cancel, on both drain paths
  (bugs F4/F16).
- `handle_logout` now takes `spawn_lock` (edge F11).
- The restart budget resets after a >60s session (bugs F7/edge F15).
- Credential DB 0600 on unix; legacy `.user_creds` dir deleted, not renamed
  (security F8/F7).
- Dead ducking machinery removed, overlay mix preserved; overlay push bounded
  (bugs F15/comments F1, security F10).
- yt-dlp stderr no longer leaked to the requester (security F5).

## Still deferred — decide at release or fold into the nob port

None release-blocking; the riskier ones need live testing, the rest are cleaner
in nob's module boundaries.

### Priority-queue / feeder path (needs live audio testing)
- **Concurrent `/play` drain race** (bugs F5, edge F9): two `/play` before
  `active_priority_item` is set both spawn drains into one bridge; the second
  overwrites `feeder_cancel`. Needs a synchronously-set guard flag.
- **`/play` join path unreachable on a fresh boot** (bugs F6, edge F10):
  `user_in_bot_voice_channel` is false when the bot is in no channel; gate
  `/play` on user-in-a-channel so it can trigger the join.
- **Feeder pacing on resume** (edge F6, bugs F9): the pause loop doesn't rebase
  `start`, so on resume it reads at full speed until the bridge drops overflow.
- **Kokoro socket calls have no timeout** (edge F8): a wedged daemon freezes the
  queue; add socket timeouts and install the cancel token before awaiting.
- `EndOfTrack` sends Idle + `bridge.clear()` every track, trimming tail audio
  (edge F20) — entangled with the eot→queue coordination; verify with the queue.

### Crypto / storage hardening (defer to nob's storage rebuild)
- Reject `V_PLAIN` rows when a key is set, and bind ciphertext to its owner via
  AAD = `discord_user_id` (security F3).
- Stretch the KDF (currently a single `Sha256`) (security F8 part).

### Misc lows
- CSRF state skipped on a bare-code paste (security F4, edge F21) — PKCE
  verifier binding already mitigates; require state for defense-in-depth.
- `pending_auth` TTL sweep (security F9); `--remote-components ejs:github` runs
  unpinned remote JS (security F11 — operationally intended, confirm).
- `/announce` toggle only gates the Spotify announcement, not priority-item ones
  (bugs F12, edge F17) — needs threading `announce_enabled` through the queue.
- Hardcoded `/opt/openclaw/...` paths for cookies, DJ clips, Kokoro socket —
  should be config (structure lens). Reconcile with the nob port.
- Comment accuracy nits (comments lens F3 dj.rs doc, etc.).

## Note for the nob port

Several of these (queue races, `/play` join, feeder pacing, crypto AAD/KDF,
hardcoded paths) are cleaner to fix inside nob's `player`/`queue`/`spotify`
module boundaries than to retrofit into spotibot's flat `bot.rs`. See `PORT.md`.
The full machine-readable report is archived with the workflow run.
