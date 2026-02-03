# Components Overview

This document explains the main pieces of the app at a high level. It is intended for contributors; end users should start with README.md.

## Discord Voice Path (Serenity + Songbird)
- The bot logs in with your Discord bot token and connects to a single guild/channel.
- Songbird joins the target voice channel and plays a raw PCM stream.
- There are no text commands; the bot only handles voice.
