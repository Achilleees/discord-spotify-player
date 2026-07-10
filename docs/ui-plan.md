# Backburner UI Plan

A future desktop tray UI. The first-run setup wizard part is already
implemented (`src/setup.rs`); only the tray window below remains hypothetical —
and since spotibot is being folded into nob (VPS-deployed, controlled from
Discord), a desktop UI is unlikely to ship. Kept as a design note.

## Goals
- Provide a tray icon that toggles a small window while the app runs.
- Expose real-time EQ controls (preamp, bass, treble) without restarting.
- ~~Guide first-time users through Discord bot setup~~ (done: `--setup` wizard).

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
- README: add UI usage section.
- Note: no `DISCORD_CLIENT_ID` env var is needed — the wizard derives the app id
  via `http.get_current_application_info()`.
