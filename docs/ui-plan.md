# Backburner UI Plan

This is a future plan for a tray UI and first-run wizard. It is not implemented yet.

## Goals
- Provide a tray icon that toggles a small window while the app runs.
- Expose real-time EQ controls (preamp, bass, treble) without restarting.
- Guide first-time users through Discord bot setup and configuration.

## Hosting Model
- Local per-user hosting only.
- Each user runs the app on their own machine with their own Discord bot token.
- The app connects one bot to one server/channel at a time.

## UI Scope (First Iteration)
- Tray icon with: Show/Hide, Quit.
- Window with:
  - EQ sliders for preamp/bass/treble.
  - Status indicators (Discord connected, Spotify connected, current track).
  - Setup wizard with links to Discord Developer Portal and invite URL.

## Config Model
- Add a local settings file (JSON/TOML) in the user config directory.
- Keep .env as an optional CLI fallback.
- Validate inputs and surface errors in the UI.

## Audio Safety
- EQ updates must be lock-free in the hot path.
- Use atomics for shared EQ parameters; recompute filters only on change.

## Threading Model
- UI event loop on the main thread.
- Tokio runtime in a background thread for Discord + Spotify.
- UI and runtime communicate via channels.

## Docs Updates (When Implemented)
- README: add UI usage section and first-run instructions.
- .env.example: add optional DISCORD_CLIENT_ID for invite URL generation.
