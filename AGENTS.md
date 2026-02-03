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
