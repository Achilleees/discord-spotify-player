> **2026-08-29:** decision #5 (OAuth PKCE + paste-back) is superseded by the
> device authorization flow — see PORT.md. The rest of this document is a
> historical record.

# Spotibot v0.5 — Hardening & Port-Readiness Plan

**Written:** 2026-07-10 · **Basis:** full 8-lens audit of `main@9ae78db` (124 confirmed findings, ~30 unique issues) + 12 locked design decisions.
**End state:** spotibot is the complete, hardened, tested, documented reference implementation of the music stack — proven live — ready to transplant into `never-off-beat` (nob) Phase 1c.

> **Status (2026-07-16): executed through Phase H.** The `v0.5-hardening`
> branch is fully merged; `main` is at v0.5.0-rc2 and live on the VPS. The
> working rules below are historical, not in force. Deviations from the plan
> are annotated inline (decisions 8/C5, D6).

---

## Locked decisions (2026-07-10)

| # | Decision | Choice |
|---|----------|--------|
| 1 | Strategy | Harden spotibot fully first, prove it live, then port to nob |
| 2 | `feat/youtube-playback` branch | **Merge everything** (YouTube, mixed queue, DJ TTS + its fixes) |
| 3 | Quality bar for live service | **Full burn-down** of all audit findings |
| 4 | Authorization model | nob's rule everywhere: **in bot's voice channel = can control** |
| 5 | OAuth | **Authorization Code + PKCE** (no client secret), hardened paste-back (validated state, tolerant parser, modal input) |
| 6 | Token storage | **SQLite, encrypted** with a key from env (0600 secrets file), replaces `.user_creds/` JSON |
| 7 | Discovery (mDNS) mode | **Delete.** OAuth-only, like nob. Kills P0 #2 by removal |
| 8 | Token refresh | **Proactive** (expires_in − margin) **+ 401-retry fallback** on Web API calls — *superseded: shipped as proactive refresh + a Notify early-refresh signal (PORT.md decision 8); the 401-retry wrapper was never implemented* |
| 9 | Tests | nob-style, portable (incl. clock abstraction for pacing) |
| 10 | Docs | **Full rewrite** of all 6 docs + `PORT.md` transfer dossier |
| 11 | Songbird/DAVE | **Migrate to songbird 0.6 stable** (same stack as nob; kills mutable fork pin) |
| 12 | Kickoff | Plan → merge → security fixes in one session; every push needs explicit go |

## Working rules for the whole effort

- **Work happens on branch `v0.5-hardening`; `main` stays untouched** until a slice is validated. The VPS auto-deploys `main` every 5 min — so `main` must only ever receive release-quality merges. Pushing the work branch never deploys anything.
- Every phase ends compilable (`cargo check` clean) and committed. Conventional commits.
- No push of any kind without an explicit go from Achille in the moment.
- Findings from the audit are referenced as `lens/Fn` (see audit report 2026-07-10).

---

## Phase A — Merge & baseline

The `feat/youtube-playback` branch (+1800 lines: yt-dlp/ffmpeg pipeline, smart /play, mixed queue, DJ announcer via Kokoro TTS, P0/P1 fix commits) becomes part of the hardening scope.

- A1. Create `v0.5-hardening` from `main`; merge `origin/feat/youtube-playback`; resolve conflicts.
- A2. `cargo check` + build clean; version bump to `0.5.0-pre`.
- A3. Diff review of conflict zones only (full re-audit comes in Phase H).
- A4. Reconcile audit list: the branch already fixes login Defer (bugs/F6), Spirc::new timeout, ever_played guard — mark those done; everything else carries over.

**Gate:** compiles, boots locally, config loads.

## Phase B — Immediate security (before anything else lands)

- B1. `.gitignore`: add `.user_creds/` (security/F1, structure/F1). Verify nothing sensitive is tracked.
- B2. Remove hardcoded channel snowflake fallback at `config.rs:72` — `TEXT_CHANNEL_ID` becomes required-or-fallback-to-`DISCORD_CHANNEL_ID`; wizard writes it; `.env.example` documents it (bugs/F3, docs/F23, structure/F3).
- B3. Track-ID strict validation (`[A-Za-z0-9]{22}`) — kills query-param injection (security/F9) AND locale-link rejection (bugs/F13) in one move.
- B4. Redact-or-remove `Debug` on `UserCredentials`; stop logging token-adjacent values (security/F2 part 2).
- B5. Un-ignore `.cargo/config.toml` (cmake fix must reach fresh clones — structure/F2); untrack `build.bat` machine paths (structure/F8).

**Gate:** security quick wins committed; candidate for first validated merge to main if we want the VPS protected early.

## Phase C — Core restructuring (one push-sized slice each)

- C1. **Songbird 0.6 migration.** Replace the beerpsi `davey` fork with songbird 0.6 stable + serenity 0.12.5 (exact nob stack — compat proven by nob's Cargo.lock). Real-voice DAVE test before calling it done (security/F10).
- C2. **Delete discovery mode.** Remove `run_discovery`, `librespot-discovery` dep, and the mDNS surface (security/F8). Single session path = `run_with_token`. Rewire **auto-start through the same machinery as `/login`** (spawn_session → voice join → bridge reader → refresh loop) — fixes P0 #1 (no sound after restart: bugs/F1-F2, edge/F3, seams/F1-F2) structurally instead of patching two half-broken paths.
- C3. **SQLite introduction.** `rusqlite` 0.32 (nob's version), single DB file, nob-style migration runner (PRAGMA user_version, ~20 lines). `credentials` table with encrypted token columns (key from `TOKEN_ENC_KEY` env; XChaCha20-Poly1305 via the `chacha20poly1305` crate — small, audited, no ring dependency). One-time migration: import `.user_creds/*.json`, then delete files. Atomic writes and corrupt-data surfacing come free (edge/F10-F11, tc/F14).
- C4. **OAuth → PKCE + hardened paste-back.** Drop client-secret requirement; code_verifier/challenge; per-user pending-state store with expiry; state validated on paste-back (bugs/F14, security/F6); parser handles schemeless URLs and `?error=access_denied` (bugs/F15, edge/F17, tc/F5); modal input for the redirect URL; Defer on all slow interactions (from branch merge).
- C5. **Token refresh architecture.** Proactive per-session refresh task driven by `expires_in` minus ~5 min margin (edge/F6, tc/F4, seams/F6); 401-retry-after-refresh wrapper around every Spotify Web API call; rotated tokens persisted atomically via C3. *Superseded: shipped without the 401-retry wrapper — the librespot task instead fires a Notify early-refresh signal on session death (see PORT.md decision 8).*
- C6. **Session lifecycle correctness.** Close the concurrent-`/login` race with compare-and-swap semantics on `active_session` (edge/F2, F9); deactivate the displaced user on takeover (bugs/F8, edge/F20); clear `ActiveSession` + presence on session death; scope empty-channel deactivation to the session owner (tc/F18).

**Gate per slice:** compiles, unit-smoke locally, committed separately.

## Phase D — Authorization + UX

- D1. **nob rule:** must be in the bot's voice channel to use playback buttons, `/queue`, and `/login`-eviction; implemented as one reusable check (security/F3-F5, decision #4).
- D2. `/logout` gated on ownership before any side effect — no more pausing the active DJ or wiping controls as a bystander (bugs/F4, edge/F4); correct double-logout reporting (bugs/F12).
- D3. Surface failures: ephemeral error replies on button/API failures instead of silent `Acknowledge`; non-success HTTP at `warn` (edge/F5, tc/F16).
- D4. Controls lifecycle: broaden startup cleanup match (bugs/F11), guard Ready re-dispatch from clobbering live controls (edge/F7), fix unreachable "is playing" embed arm (bugs/F10), track-dedup by `track_id` + clear on pause (tc/F17).
- D5. Stage channels: restore unsuppress via `EditVoiceState` after join (wizard offers stages, so support them properly — bugs/F7, seams/F4).
- D6. `/who` gated by the same in-channel rule (security/F7). *Dropped: `/who` shipped ungated (it only names the active DJ); code and README both treat it as ungated.*

## Phase E — Robustness

- E1. Ring buffer: round drop/fill to stereo-frame boundaries on producer, consumer, and reader zero-fill (tc/F1); guard sub-4-byte reads from returning EOF (bugs/F16, tc/F11).
- E2. Wizard: fix post-wizard env re-read (explicit override of process env — edge/F8); exact-key `.env` rewrite instead of prefix match (tc/F15).
- E3. Config: reject zero IDs cleanly (edge/F15); warn on present-but-unparseable numeric vars (edge/F18).
- E4. Player loop: preserve reconnect retry budget on `Spirc::new` failure (edge/F12); cap the outer refresh-reconnect loop (no more 2 s hot loop forever); review the 5-consecutive-short-sessions refresh trigger (tc/F13).
- E5. **PREBUFFER_SECONDS: wire it** — `read()` honors `prebuffer_samples`/`prebuffer_wait` (bugs/F5, seams/F3, comments/F1/F3).
- E6. Dead-surface sweep: `configured_channel_kind`, unused `EditVoiceState` import (restored by D5 or deleted), `paired_at` (surface in `/who` or drop), `AudioBridge::default`, `librespot` umbrella dep, unused `Serialize`/`expires_in` leftovers (seams/F5-F8, bugs/F17).

## Phase F — Tests (nob-style, portable)

- F1. Adopt nob's test conventions and layout; add dev-deps.
- F2. Pure-logic units: track-ID parser (locale, URI, injection cases), `extract_code` (schemeless, error param, raw code), `truncate_status` boundaries, `pct_encode`, config clamps, biquad coefficients (clamp must not mask instability), ring buffer parity/wraparound/drop, join-sound shape.
- F3. Clock abstraction over `Instant::now()` in the sink; pacing tests (tc/F10).
- F4. Token expiry/refresh decision logic extracted pure + tested (tc/F4).
- F5. Gate: `cargo test` green, `cargo clippy` clean.

## Phase G — Docs (full rewrite) + PORT.md

- G1. `README.md` — v0.5 reality: OAuth login, slash commands, YouTube/SoundCloud, DJ, queue, controls, intents/permissions required.
- G2. `CLAUDE.md` + `AGENTS.md` — current architecture, startup branching, dependency truth (songbird 0.6), conventions; stale roadmap sections dropped.
- G3. `docs/components.md` rewrite (all modules, correct log defaults); `docs/ui-plan.md` pruned to what's still future.
- G4. `.env.example` complete: `SPOTIFY_CLIENT_ID`, `TEXT_CHANNEL_ID`, `TOKEN_ENC_KEY`, all knobs (no client secret anymore — PKCE).
- G5. **`PORT.md` — the transfer dossier:** module-by-module mapping to nob (`spotify/` → `nob-music/spotify`, sink → nob-audio adapter, OAuth client → nob-music, credentials table → nob schema), the paid-for gotchas (DAVE story, Spirc lifecycle, pacing math, paste-back UX rationale), today's 12 decisions, and explicitly what NOT to port (bot.rs UI — nob's panel supersedes it).
- G6. Comment pass: fix the 5 inaccurate comments, convert prose invariants to `debug_assert!`, trim rationale/history narration (comments/F1-F14).

## Phase H — Validation, second audit, release

- H1. **Audit #2** (same 8-lens workflow) on the merged + hardened tree — the branch's +1800 lines were never audited, and the hardening itself needs adversarial eyes.
- H2. End-to-end verify in a real voice channel: login (PKCE paste-back), playback, buttons under the in-channel rule, `/queue`, YouTube, DJ announce, **service-restart auto-start test** (the original P0).
- H3. Burn down audit #2 findings; then release: version `0.5.0`, tag, merge `v0.5-hardening` → `main`, push (explicit go), watch the VPS deploy + `journalctl` soak.

## Phase I — Port to nob (Phase 1c execution)

- I1. Update nob's `TASKS.md`/`ARCHITECTURE.md` first: record decisions #4-8 (PKCE paste-back UX, encryption scheme, refresh architecture, in-voice rule) and amend "Spotibot is reference only" to "hardened spotibot modules transplant with adaptation".
- I2. Transplant order: OAuth client + credentials storage → librespot session lifecycle + sink (adapt to nob-audio's tested bridge) → refresh task → Spotify commands mapped onto nob's actions/panel layer → priority-model integration (Spotify Connect = baseline layer 3).
- I3. Port the F-phase tests alongside each module.
- I4. nob Phase 1c acceptance: everything spotibot v0.5 does, inside nob, plus the priority model.
- I5. Deployment switch on the VPS: nob service replaces spotibot; spotibot repo archived with `PORT.md` as its tombstone.

---

## Audit findings → phase map (unique issues)

| Issue | Phase |
|---|---|
| Auto-start silent (P0 #1) | C2 |
| Discovery broken (P0 #2) | C2 (deleted) |
| Hardcoded snowflake | B2 |
| `.user_creds` gitignore / perms / Debug | B1, B4, C3 |
| No auth gates | D1, D6 |
| Stale token in healthy session | C5 |
| CSRF state unused | C4 |
| Track-ID injection + locale links | B3 |
| Songbird mutable pin | C1 |
| `/logout` side effects, login race, reactivation no-op | C6, D2 |
| Login >3s / Defer | A (from branch) |
| Stage suppression | D5 |
| Silent button failures | D3 |
| Controls lifecycle bugs | D4 |
| Wizard env re-read / prefix match | E2 |
| Store atomicity/corruption | C3 |
| Ring buffer parity / EOF | E1 |
| Config validation | E3 |
| Retry budget / hot loop | E4 |
| PREBUFFER dead knob | E5 |
| Dead surface | E6 |
| Comment accuracy | G6 |
| Docs drift (23 findings) | G1-G5 |
| Zero tests | F |
