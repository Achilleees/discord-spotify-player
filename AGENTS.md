# AGENTS.md

This repo is a Discord voice bridge for Spotify Connect. Use these notes when making changes.

## Safety and secrets
- Never print or paste values from `.env` in chat.
- Keep `.env` and `.spotify_cache/` local-only; do not add secrets to README.
- Prefer documenting settings in `.env.example`.

## Build and run
- Prefer `cargo build --release` for performance-sensitive changes.
- The release binary is `target\release\discord-spotify-player.exe`.

## Audio/perf
- Avoid allocations and heavy logging in the hot audio path.
- Use `RUST_LOG` to enable debug logs; default should stay quiet.
- Keep Spotify device IDs stable to avoid duplicate devices.

## Behavior
- This bot exposes a Spotify Connect device and routes audio into one Discord voice channel.
- No text commands are expected; avoid adding them unless requested.

## Documentation
- README should explain what the bot does and expectations, not implementation details.
- Do not include any user-specific identifiers or tokens.

## Known Issues

### Spotify Disconnects Every ~2 Minutes (librespot 0.4.x keepalive bug)
- **Symptom**: `subscription terminated` and `os error 10054` in logs, Spotify client loses connection
- **Root cause**: librespot 0.4.x has a keepalive ping/pong bug. Fixed in 0.5.0 via PR #1359.
- **Why we can't upgrade**: librespot 0.5+ has a vergen ecosystem bug - `vergen-gitcl 1.0.x` pulls in conflicting `vergen-lib` versions (0.1.6 and 9.1.0). This affects all librespot versions 0.5.0 through 0.8.0.
- **Attempted fixes** (Feb 2025): Tried 0.5.0, 0.7.1, 0.8.0, git dev branch, various patch configurations. All fail with same vergen conflict.
- **Workaround**: App reconnects automatically (up to 5 attempts). User may need to re-select device in Spotify after reconnect.
- **Future**: Monitor vergen-gitcl releases for fix, then retry librespot upgrade.

### Librespot Legality
- Gray area - reverse-engineered Spotify protocol, technically violates ToS
- Requires Spotify Premium (legitimate paid account)
- No audio extraction/DRM bypass (intentionally limited)
- Project has operated 10+ years without legal action
- See: https://github.com/librespot-org/librespot
