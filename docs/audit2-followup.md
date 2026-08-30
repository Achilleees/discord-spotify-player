# Audit #2 — follow-up (deferred findings)
> Historical record from the v0.5 audit cycle, kept for provenance. The architecture it describes was superseded by the player-core refactor — see PORT.md.

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

## Also fixed on `fix/audit2-followup` (after commit a57ac8b "chore: release v0.5.0"; no v0.5.0 tag exists — the version was walked back to rc for live VPS testing)

- Kokoro socket exchange bounded by a 20s timeout (edge F8).
- `/play` reachable on a fresh boot — gated on user-in-a-channel when the bot
  isn't in voice yet (bugs F6, edge F10).
- Feeder rebases its pacing clock by the paused duration on resume (edge F6).
- `/announce` toggle now gates priority-item announcements too (bugs F12).
- `pending_auth` reaps expired entries on insert (security F9).
- Priority queue capped at 500 with unit tests (security F2).
- Corrected the `track_announce_clip` doc comment (comments F3).
- **Concurrent `/play` drain race fully serialized** (bugs F5, edge F9): a
  single abort-safe `drain_active` guard makes exactly one drain own the queue
  at a time, across both the `/play` trigger and the eot-driven manager. This
  closes the two-drain-path race completely — it did NOT need the nob rebuild.
- **Credential ciphertext bound to its owner via AEAD AAD + plaintext-downgrade
  rejection** (security F3). Nothing deployed → no encrypted data to migrate.
- **`EndOfTrack` no longer clears the bridge** (edge F20) — the natural-boundary
  tail-trim is gone; real stops go through `Stopped`, and a priority item's
  drain clears the bridge itself. (The brief presence flap is cosmetic; left
  as-is to keep the change low-risk.)
- **Deployment paths are env-configurable** (structure lens): DJ clips/cache,
  Kokoro socket, YouTube tmp dir + cookies default to the VPS layout but can be
  overridden. Documented in `.env.example`.
- **`TOKEN_ENC_KEY` stretched with PBKDF2-HMAC-SHA256** (600k iters, fixed salt;
  security F8) — a weak key is now costly to brute-force from a stolen DB.
  pbkdf2/hmac were already in the tree, so zero new compiled deps.

## Still open — genuinely gated (not by effort)

None release-blocking. Every code-actionable finding is fixed; these three each
have a concrete, non-effort blocker:

- **CSRF state on a bare-code paste** (security F4): **accepted.** PKCE verifier
  binding already mitigates — an attacker's code is useless without our stored
  verifier — and requiring state would break the paste-just-the-code UX. Fixing
  it would make the product worse for a threat that's already covered.
- **`--remote-components ejs:github`** (security F11): **your operational call.**
  This is how yt-dlp handles YouTube's current signature challenges; pinning or
  removing it could break YouTube extraction. Changing your extractor setup
  unilaterally is riskier than leaving it.
- **Presence flap on Spotify auto-advance** — the residual cosmetic half of F20
  (bot status briefly Idle between auto-advanced tracks). Needs live audio to
  verify and tune the presence/reader timing.

## Note for the nob port

The queue/player coordination (two drain paths, EndOfTrack↔queue) is cleaner as
nob's single player-state machine than retrofitted into spotibot's flat
`bot.rs`. See `PORT.md`.
The full machine-readable report is archived with the workflow run.
