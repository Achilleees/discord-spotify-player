# Running Spotibot and nob

Both executables use the same music implementation. Each process owns its
Discord client, voice connection, player actor, queue, Spotify login, card
and state. Nob additionally offers `/slowmode` and `/purge`. Soundboard,
companion behavior and the remaining legacy server modules follow separately.

## Build and configure

```sh
cargo build --workspace --release --locked
```

This builds `target/release/discord-spotify-player` and `target/release/nob`
(append `.exe` on Windows). The default root build/run still selects Spotibot;
`cargo run -p nob -- --help` selects nob explicitly.

Keep Spotibot's existing `.env`. Copy `.env.nob.example` to `.env.nob` and
configure nob with a different Discord application/bot token. Use the same
guild and choose a voice and text channel for each bot. Keys in the files
are unprefixed. For service environment variables, Spotibot accepts the
existing names; nob accepts only `NOB_*` names such as `NOB_DISCORD_TOKEN`,
`NOB_DISCORD_GUILD_ID`, `NOB_DISCORD_CHANNEL_ID` and `NOB_STATE_DIR`.

```sh
target/release/discord-spotify-player --check-config
target/release/nob --check-config
```

These commands validate configuration without connecting, probing external
tools or writing state. They do not authenticate tokens or test permissions.
`--env-file PATH` explicitly selects another env file; no default file or
parent-directory search is performed in that case. `--help` reads no config.
The interactive `--setup` wizard belongs to Spotibot and writes `.env`;
nob fails on missing configuration instead of opening that wizard.

## State and services

By default, nob stores its database, generated Spotify device identity,
download scratch files and DJ cache beneath `.nob` in the process working
directory. Spotibot retains its previous default paths. Set `STATE_DIR`
(or `NOB_STATE_DIR` for nob's service environment) to choose a different
instance root. Relative path overrides are resolved beneath this directory;
absolute paths stay absolute. Moving it selects new state; it does not move
an existing database. See [configuration.md](configuration.md) for overrides.

Run two services with separate credentials, state directories and restart
policies. Leave `DEVICE_ID` unset to generate a persistent identity per cache,
or deliberately configure different IDs. Use separate token-encryption keys.
Do not point both services at the same database, Spotify cache, scratch
directory, cookie file or DJ cache. Startup locks refuse concurrent use of these writable
resources by current builds. Lock files persist, but their locks release on
process exit; do not delete them while a service may be running. Older builds
do not participate in these locks and still require separate paths.

Each bot has its own Spotify login. Simultaneous Spotify playback needs
separate eligible accounts; two Discord identities alone do not provide a
second Spotify stream. Independent YouTube/file playback uses each bot's
own queue and voice connection. DJ clips and a Kokoro endpoint can be configured explicitly; generated
audio must use separate cache paths. yt-dlp uses `extractor-cache` under its
scratch directory and ignores ambient yt-dlp configuration for both metadata
and playback. Its [cookie option](https://github.com/yt-dlp/yt-dlp#filesystem-options)
can write the jar back, so each bot needs its own cookie file as well.

## Discord setup and acceptance

Grant both bots the music permissions described in
[getting-started.md](getting-started.md). Nob's additional commands check
both the caller's and bot's channel permissions: Manage Channels for
slowmode; Manage Messages, View Channel and Read Message History for cleanup.
No privileged gateway intent is added for these utilities. Select the intended
bot in Discord's slash-command picker, or use that bot's own playback card.

Before a live rollout, verify separate logins and queues, independent audio
in two rooms, and that stopping/restarting one bot leaves the other running.
Also check nob's moderation permissions and the shared private music menus.
Local tests cover configuration isolation, locks, stable device identities,
permission decisions and command boundaries; live Discord acceptance remains
a separate step. Deployment continues through an explicitly selected, green
`dev` commit promoted to deployment-only `main`.
