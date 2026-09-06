# spotibot

A self-hosted Discord music bot in Rust. It runs as a Spotify Connect device
for one DJ at a time and streams that audio into a voice channel, alongside
YouTube, SoundCloud and uploaded files — all in one shared queue that plays in
strict order, like a radio. Control from any Spotify client or from slash
commands and now-playing buttons in Discord.

This repository is the shared home for **Spotibot and nob**, continuing
Spotibot's Git history. Two executables share the music engine, playback card
and private search menus. Spotibot specializes in music; nob also offers
permission-checked slowmode and message cleanup. Each runs in its own process
with separate configuration and state. The eventual project name is
`never-off-beat`; see [docs/PORT.md](docs/PORT.md).

## Quick start

```bash
cargo build --release
target/release/discord-spotify-player --setup   # first run: writes .env
target/release/discord-spotify-player
```

Then in Discord: `/login`, open spotify.com/pair, enter the code. Pick the bot
in Spotify's device list (or it's picked for you on `/login`) and play.

Needs a Discord bot token, a guild and a voice channel; Spotify Premium for the
DJ; `yt-dlp` + `ffmpeg` on `PATH` for non-Spotify sources (optional).

To build both bots, use `cargo build --workspace --release --locked`.
Configure nob from `.env.nob.example`, then run `target/release/nob`
(`nob.exe` on Windows). See [running both bots](docs/two-bots.md) for identity,
state paths and offline configuration checks.

## Documentation

| | |
|---|---|
| [docs/getting-started.md](docs/getting-started.md) | prerequisites, build, Discord setup, first login, running as a service |
| [docs/commands.md](docs/commands.md) | every slash command and button, who may use it, how the queue orders playback |
| [docs/configuration.md](docs/configuration.md) | every environment variable, logging, where state lives |
| [docs/architecture.md](docs/architecture.md) | how it works: audio pipeline, the player actor, the three lifecycles |
| [docs/troubleshooting.md](docs/troubleshooting.md) | symptoms → causes → fixes |
| [CODEMAP.md](CODEMAP.md) | technical map of `src/` |
| [CHANGELOG.md](CHANGELOG.md) | what changed, per release |

Contributors and agents: [AGENTS.md](AGENTS.md) / [CLAUDE.md](CLAUDE.md).

## Privacy

Per-user Spotify tokens live in a local SQLite database (`spotibot.db`),
encrypted at rest when `TOKEN_ENC_KEY` is set. The bot never calls Spotify's
Web API. Not affiliated with Spotify or Discord; personal, non-commercial use —
you are responsible for complying with Spotify's terms.

## License

See [LICENSE](LICENSE).
