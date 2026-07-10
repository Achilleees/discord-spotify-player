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

## Also fixed on `fix/audit2-followup` (post-v0.5.0 tag)

- Kokoro socket exchange bounded by a 20s timeout (edge F8).
- `/play` reachable on a fresh boot — gated on user-in-a-channel when the bot
  isn't in voice yet (bugs F6, edge F10).
- Feeder rebases its pacing clock by the paused duration on resume (edge F6).
- `/announce` toggle now gates priority-item announcements too (bugs F12).
- `pending_auth` reaps expired entries on insert (security F9).
- Priority queue capped at 500 with unit tests (security F2).
- Corrected the `track_announce_clip` doc comment (comments F3).

## Still open — for nob's rebuild or an operational decision

None release-blocking.

- **Concurrent `/play` drain race + the two-drain-path design** (bugs F5, edge
  F9): `trigger_priority_queue_drain` and the eot-driven `priority_queue_manager`
  can both drain. A partial guard would give false confidence; nob's single
  player-state machine fixes it holistically. **Defer to the port.**
- **`EndOfTrack` sends Idle + `bridge.clear()` every track** (edge F20),
  trimming tail audio — entangled with the eot→queue coordination above.
- **Crypto**: reject `V_PLAIN` rows when a key is set + bind ciphertext to its
  owner via AAD; stretch the KDF (security F3, F8). Defer to nob's storage
  rebuild (spotibot stays sync-`Mutex<Connection>`; nob uses async `Db`).
- **CSRF state on a bare-code paste** (security F4): PKCE verifier binding
  already mitigates (an attacker's code is useless without our verifier);
  requiring state would break the paste-just-the-code convenience. Accepted.
- **`--remote-components ejs:github`** runs unpinned remote extractor JS
  (security F11) — operationally intended; **needs your call** to pin/remove.
- **Hardcoded `/opt/openclaw/...` paths** for cookies, DJ clips, Kokoro socket
  (structure lens) — make them config during the nob port.

## Note for the nob port

Several of these (queue races, `/play` join, feeder pacing, crypto AAD/KDF,
hardcoded paths) are cleaner to fix inside nob's `player`/`queue`/`spotify`
module boundaries than to retrofit into spotibot's flat `bot.rs`. See `PORT.md`.
The full machine-readable report is archived with the workflow run.
