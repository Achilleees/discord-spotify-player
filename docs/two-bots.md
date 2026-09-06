# Running Spotibot and nob

Both executables use the same music implementation. Each process owns its
Discord client, voice connection, player actor, queue, Spotify login, card
and state. Nob additionally offers slowmode and message cleanup. Soundboard,
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

## One slash menu for both bots

Paired mode is opt-in. In Spotibot's env file set:

```dotenv
COMMAND_MODE=worker
ROUTING_LISTEN=127.0.0.1:19211
```

In nob's env file set:

```dotenv
COMMAND_MODE=coordinator
ROUTING_LISTEN=127.0.0.1:19212
ROUTING_PEER=127.0.0.1:19211
```

Set `ROUTING_KEY` in **both** files to the same securely generated random
32-byte key encoded as 64 hexadecimal characters. Keep it local, and separate
from either bot's `TOKEN_ENC_KEY`. Process overrides for nob use `NOB_` prefixes.
Both addresses must be loopback IP addresses with distinct nonzero ports;
this mode connects two services on the same host. Run both offline
`--check-config` commands before restarting either service.

Nob then registers only `/play`, `/music` and `/server`. Spotibot registers
no slash commands and keeps its own playback-card buttons. `/soundboard`
will join this list when the clip feature is implemented. On startup each
paired host also removes its own known legacy global slash registrations;
guild registration alone does not remove global duplicates. Existing stale
slash invocations point users back to nob. Changing registration requires
restarting the configured host; neither bot silently changes mode on failure.
Set `COMMAND_MODE=standalone` to retain the original independent command list.

`/play` uses the bot already serving your voice room. Otherwise it chooses
free Spotibot, then free nob, considering media support. A paused bot remains
busy. If either status is unknown, both bots are in your room, or neither can
serve you, a private picker shows their rooms and availability. Choosing a
busy bot does not move it: playback still requires joining its room. A bare
`/music` opens an existing session or asks which bot to inspect.

The private music panel offers playback, Add music/search, queue, history,
Spotify login/logout/forget and announcements. It names the selected bot;
accounts and queue state belong to that performer. Account inspection and
removal work outside voice. Pairing outside voice saves the login without
activating it; join voice and use **Log in** again to activate. If the room
or session changes during login, track lookup or a delayed click, open a
fresh panel. Clear queue and Forget login require confirmation. Previous,
Pause, Skip and Stop target an existing connection and cannot summon a spare.
`/server` opens nob's permission-checked tools for the current text channel.

Requests carry an instance/session revision and a unique ID. When a reply is
lost, nob queries that request's outcome; it never repeats the action on
another bot. An unknown result asks you to check playback before retrying.
If nob goes offline, existing music and each performer's own card controls
continue working; new centralized slash requests wait for nob to return.

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
No privileged gateway intent is added for these utilities. In paired mode use
nob's private picker, or use the intended performer's own playback card.

Before a live rollout, verify separate logins and queues, independent audio
in two rooms, and that stopping/restarting one bot leaves the other running.
Also check nob's moderation permissions and the shared private music menus.
For paired mode, inspect both apps' guild and global command lists; verify
one slash surface, Stop during join/login, moving rooms during lookup,
expired menus, account removal outside voice and coordinator/worker restarts.
Confirm that a lost acknowledgment never starts music on the other bot.
Local tests cover configuration isolation, locks, stable device identities,
permission decisions and command boundaries; live Discord acceptance remains
a separate step. Deployment continues through an explicitly selected, green
`dev` commit promoted to deployment-only `main`.
