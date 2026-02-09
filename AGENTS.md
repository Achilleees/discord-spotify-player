# AGENTS.md

This repo is a Discord voice bridge for Spotify Connect. Use these notes when making changes.

## Safety and secrets
- Never print or paste values from `.env` in chat.
- Keep `.env` and `.spotify_cache/` local-only; do not add secrets to README.
- Prefer documenting settings in `.env.example`.

## Build and run
- Prefer `cargo build --release` for performance-sensitive changes.
- The release binary is `target\release\discord-spotify-player.exe`.
- First-run setup can be launched with `--setup`.

## Audio/perf
- Avoid allocations and heavy logging in the hot audio path.
- Use `RUST_LOG` to enable debug logs; default should stay quiet.
- Keep Spotify device IDs stable to avoid duplicate devices.

## Logging policy
- All tracing messages should be lowercase (for example: `tracing::info!("configuration loaded")`).
- Use structured tracing fields instead of format strings (for example: `tracing::debug!(samples = n, "push_samples")`).
- Sink start/stop are `debug`, not `info`. Reserve `info` for startup milestones and connection events.
- The `audio_stream` target is used for high-frequency audio diagnostics; gate these behind `debug` or sampled counters.

## Dependency policy
- `rand` is at 0.10. Use `rand::random::<T>()` (no `Rng` trait import needed for simple cases).
- `librespot` is at 0.8 with pinned vergen workaround in build-deps. Check upstream before bumping.
- Prefer `parking_lot::Mutex` over `std::sync::Mutex` for non-async locks (shorter critical sections, no poisoning).

## Behavior
- This bot exposes a Spotify Connect device and routes audio into one Discord voice channel.
- No text commands are expected; avoid adding them unless requested.

## Documentation
- README should explain what the bot does and expectations, not implementation details.
- Do not include any user-specific identifiers or tokens.

## Librespot Version

Using librespot **0.8.0** which includes the keepalive fix (PR #1359) for stable connections.

### Upgrade Notes (Feb 2026)
- The vergen-lib version conflict that blocked upgrades was fixed by pinning `vergen = "=9.0.6"` and `vergen-gitcl = "=1.0.5"` in build-dependencies.
- See https://github.com/librespot-org/librespot/issues/1681 for the upstream issue.

### Librespot Legality
- Gray area: reverse-engineered Spotify protocol, technically violates ToS.
- Requires Spotify Premium (legitimate paid account).
- No audio extraction/DRM bypass (intentionally limited).
- Project has operated 10+ years without legal action.
- See: https://github.com/librespot-org/librespot

## Roadmap

Feature branch status is tracked here and in README.

### feat/now-playing-channel
**Status:** Planned

**Goal:** Text channel with rich embeds and playback controls.

Implementation notes:
- Add optional `DISCORD_NOW_PLAYING_CHANNEL_ID` to config.
- Create embed builder for track info (title, artist, album art, Spotify link).
- Use Discord message components (buttons) for play/pause/skip.
- Keep one "sticky" message that gets edited on track change.
- Wire button interactions to Spirc commands.
- Fetch album art URL from librespot metadata.

Dependencies: None (serenity already supports embeds and components)

### feat/setup-wizard
**Status:** Complete (ready to merge to `main`)

**Goal:** Interactive CLI for first-run configuration.

Implementation notes:
- Detect missing/incomplete `.env` on startup.
- Add `dialoguer` prompts for token, guild/channel selection, and device name.
- Use Discord REST API (via serenity HTTP client) to list guilds and channels.
- Flow: prompt token -> validate -> list guilds -> pick guild -> list voice channels -> pick channel -> write `.env`.
- Generate bot invite URL with Connect/Speak permissions.
- Preserve existing `.env` comments/keys where possible when updating values.

Dependencies: `dialoguer`

### feat/youtube-support
**Status:** Planned

**Goal:** Play YouTube audio alongside Spotify.

Implementation notes:
- Use `yt-dlp` as external binary for extraction (avoid linking ffmpeg).
- Add slash command or message trigger (`!yt <url>` or `/play <url>`).
- Extract audio URL, stream through same AudioBridge.
- Handle Spotify vs YouTube source switching and queue behavior.
- Consider YouTube search via `yt-dlp --default-search ytsearch:`.
- Legal: YouTube ToS is stricter than Spotify; document risks.

Dependencies: `yt-dlp` binary, possibly `symphonia` for additional codecs

### Branch workflow
- Each feature branch starts from `main`.
- Merge to `main` when feature is complete and tested.
- Update README roadmap status when merging.
