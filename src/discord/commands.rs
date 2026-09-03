//! Slash-command registration and dispatch: turning a Discord interaction
//! (a slash command or a control-card button) into a gated call onto the
//! player actor, or into one of the account-lifecycle methods in
//! `discord::account`.
//!
//! `Handler`'s struct definition and its `EventHandler` impl live in
//! `bot.rs`; `EventHandler::interaction_create` there is a one-line
//! delegate into `dispatch_interaction` below.

use super::account::LoginOutcome;
use super::bot::Handler;
use crate::player::state::{EnqueuePos, NowPlaying};
use crate::queue::{MediaSource, QueueItem};
use crate::spotify::EnsureOutcome;
use crate::youtube::metadata::{fetch_youtube_metadata, validate_attachment};
use serenity::all::{
    ChannelId, CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage,
    Interaction, UserId,
};
use serenity::builder::{
    CreateActionRow, CreateButton, CreateCommandOption, EditInteractionResponse,
};
use serenity::model::application::ButtonStyle;
use serenity::client::Context;
use serenity::model::application::CommandOptionType;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(super) fn register_commands(ytdlp_available: bool) -> Vec<CreateCommand> {
    let mut cmds = vec![
        CreateCommand::new("login")
            .description("Connect your Spotify account (or reactivate existing session)"),
        CreateCommand::new("logout")
            .description("Deactivate your Spotify session (credentials kept for quick re-login)"),
        CreateCommand::new("forget")
            .description("Permanently delete your stored Spotify credentials"),
        CreateCommand::new("who")
            .description("Show whose Spotify account is currently active"),
        CreateCommand::new("queue")
            .description("Add to the queue without starting playback; no argument shows the queue")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "url",
                    "Spotify track URL/URI, or YouTube/SoundCloud URL",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::Attachment, "file",
                    "Audio file to queue (mp3, flac, ogg, wav, m4a, aac, opus, wma)")
                .required(false),
            ),
        CreateCommand::new("skip")
            .description("Skip the current track"),
        CreateCommand::new("stop")
            .description("Stop playback and leave the channel; the queue is kept"),
        CreateCommand::new("clear")
            .description("Clear the queue (asks to confirm); what's playing keeps playing"),
        CreateCommand::new("np")
            .description("Show what's currently playing"),
        CreateCommand::new("history")
            .description("Show what has played recently")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "count",
                    "How many tracks to show (1-25, default 10)",
                )
                .min_int_value(1)
                .max_int_value(25)
                .required(false),
            ),
        CreateCommand::new("announce")
            .description("Toggle DJ track announcements on/off"),
    ];

    if ytdlp_available {
        cmds.push(
            CreateCommand::new("play")
                .description("Play a Spotify/YouTube/SoundCloud URL or file; with no argument, press play")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "url",
                        "Spotify, YouTube, or SoundCloud URL")
                    .required(false),
                )
                .add_option(
                    CreateCommandOption::new(CommandOptionType::Attachment, "file",
                        "Audio file to play (mp3, flac, ogg, wav, m4a, aac, opus, wma)")
                    .required(false),
                )
                .add_option(
                    CreateCommandOption::new(CommandOptionType::Boolean, "next",
                        "Play this right after the current track")
                    .required(false),
                ),
        );
    }

    cmds
}

/// Parse a Spotify track ID from a URL or URI.
/// Accepts `spotify:track:<id>` and any `open.spotify.com` URL with a
/// `/track/<id>` path segment, including locale-prefixed links
/// (`open.spotify.com/intl-fr/track/<id>`).
fn parse_track_id_from_url(input: &str) -> Option<String> {
    let input = input.trim();
    let candidate = if let Some(rest) = input.strip_prefix("spotify:track:") {
        rest.split('?').next().unwrap_or(rest)
    } else if input.contains("open.spotify.com/") {
        let after = input.split("/track/").nth(1)?;
        after.split(['?', '/', '#']).next().unwrap_or(after)
    } else {
        return None;
    };
    is_valid_track_id(candidate).then(|| candidate.to_string())
}

/// Spotify track IDs are exactly 22 base62 characters. Anything else is
/// rejected here rather than being handed to `SpotifyUri::from_uri` and on
/// into librespot's Mercury calls, so a malformed link fails as a clear
/// "that isn't a Spotify track" instead of as a metadata lookup error.
fn is_valid_track_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Result of sorting a `/play` or `/queue` link argument into the Spotify
/// fast path or the generic YouTube/SoundCloud/attachment path.
enum LinkKind {
    Spotify(librespot_core::SpotifyUri),
    Other,
}

/// Classifies a URL/URI argument. A recognized Spotify track link resolves
/// straight to a `SpotifyUri`; anything else (including a malformed Spotify
/// link) falls through to the YouTube/SoundCloud/attachment path, which
/// reports its own "unsupported URL" error for garbage input.
fn classify_link(input: &str) -> LinkKind {
    let track_id = match parse_track_id_from_url(input) {
        Some(id) => id,
        None => return LinkKind::Other,
    };
    let uri = format!("spotify:track:{}", track_id);
    match librespot_core::SpotifyUri::from_uri(&uri) {
        Ok(u) => LinkKind::Spotify(u),
        Err(e) => {
            tracing::warn!(error = %e, uri = %uri, "failed to parse Spotify track URI");
            LinkKind::Other
        }
    }
}

/// Renders the `/history` listing, newest first. Pure so its branches —
/// the fallbacks, the requester suffix, and the length budget — can be
/// pinned by tests; the surrounding command only fetches the rows.
fn render_history(rows: &[crate::history::HistoryRow]) -> String {
    if rows.is_empty() {
        return "Nothing has played yet.".to_string();
    }
    // Discord rejects a body over 2000 characters outright, and 25 rows of
    // long titles clears that easily — so stop early and say so, the way the
    // queue listing already does.
    const MAX_BODY: usize = 1900;
    let mut out = String::from("🕘 **Recently played**\n");
    let mut shown = 0usize;
    for row in rows {
        let title = row.title.as_deref().unwrap_or("Unknown track");
        let artist = row.artist.as_deref().unwrap_or("Unknown artist");
        // `<t:unix:t>` is Discord's own timestamp markup: each reader sees
        // it in their timezone, which is what makes "what played on Tuesday"
        // answerable without the bot guessing at anyone's clock.
        // Not in backticks: Discord only expands the markup as plain text.
        let mut line = format!("• <t:{}:t> **{title}** — {artist}", row.aired_at_unix);
        if let Some(who) = row.queued_by.as_deref() {
            line.push_str(&format!(" (queued by {who})"));
        }
        line.push('\n');
        if out.len() + line.len() > MAX_BODY {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < rows.len() {
        out.push_str(&format!("…and {} more.", rows.len() - shown));
    }
    out
}

/// Reply to a button press so only the clicker sees it.
async fn respond_ephemeral(
    ctx: &Context,
    component: &serenity::model::application::ComponentInteraction,
    content: String,
) {
    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new().content(content).ephemeral(true),
    );
    if let Err(e) = component.create_response(ctx, response).await {
        tracing::warn!(error = ?e, "failed to respond to button interaction");
    }
}

/// Answer a confirmation prompt by rewriting the prompt itself and removing
/// its buttons — the outcome lands where the question was asked, and the
/// question can't be answered a second time.
async fn replace_prompt(
    ctx: &Context,
    component: &serenity::model::application::ComponentInteraction,
    content: String,
) {
    let response = CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new().content(content).components(vec![]),
    );
    if let Err(e) = component.create_response(ctx, response).await {
        tracing::warn!(error = ?e, "failed to update a confirmation prompt");
    }
}

/// The Confirm/Cancel pair shown by `/clear`.
fn clear_confirm_buttons() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new("ctrl_queue_clear_confirm")
            .label("Clear the queue")
            .style(ButtonStyle::Danger),
        CreateButton::new("ctrl_queue_clear_cancel")
            .label("Cancel")
            .style(ButtonStyle::Secondary),
    ])
}

/// Pure voice-gate policy (docs/PORT.md locked decision 4): with the bot in a
/// channel the user must share it; with the bot in none, `allow_follow`
/// decides whether being in any voice channel suffices (the /play
/// fresh-boot path, where the bot joins the requester).
fn voice_gate(
    bot_ch: Option<ChannelId>,
    user_ch: Option<ChannelId>,
    allow_follow: bool,
) -> bool {
    match bot_ch {
        Some(bc) => user_ch == Some(bc),
        None => allow_follow && user_ch.is_some(),
    }
}

/// Commands that make the bot do something audible, and so require sharing
/// its voice channel. `/announce` is a guild-level toggle rather than
/// playback control and must be settable before the bot joins; `/np`,
/// `/queue`, `/history` and `/who` only read.
fn command_drives_playback(name: &str) -> bool {
    matches!(name, "skip" | "stop")
}

/// Commands that change the queue without touching audio. These take the
/// looser gate — the caller must be in a voice channel, but not necessarily
/// the bot's, because the bot may be in none.
///
/// `/clear` is here rather than above for a reason `/stop` creates: `/stop`
/// leaves the channel and its reply says the queue survived and to use
/// `/clear`. Under the strict gate that command is refused in exactly the
/// state the message describes.
fn command_mutates_queue(name: &str) -> bool {
    matches!(name, "clear")
}

/// The button equivalents of [`command_drives_playback`].
fn button_drives_playback(custom_id: &str) -> bool {
    matches!(custom_id, "ctrl_prev" | "ctrl_next" | "ctrl_pause_toggle")
}

/// The button equivalents of [`command_mutates_queue`]. The queue-hint
/// button is read-only info and stays open; so does cancelling the prompt.
fn button_mutates_queue(custom_id: &str) -> bool {
    matches!(custom_id, "ctrl_queue_clear_confirm")
}

/// Renders the actor's view of what's audible, for `/np` and the queue
/// listing's status line. Follows the reply house style documented on
/// `player::state`'s `reply`: `▶`/`⏸` for the transport state, track and
/// user names in bold, one phrasing per state.
fn render_now_playing(now: &NowPlaying) -> String {
    match now {
        NowPlaying::Nothing => "Nothing is playing right now.".to_string(),
        NowPlaying::Media { title, subtitle, queued_by, paused } => {
            let glyph = if *paused { "⏸" } else { "▶" };
            format!("{glyph} **{title}** — {subtitle} · queued by **{queued_by}**")
        }
        NowPlaying::Spotify { title, artist, paused } => {
            let glyph = if *paused { "⏸" } else { "▶" };
            format!("{glyph} **{title}** — {artist}")
        }
        NowPlaying::SpotifyStarting => "▶ Starting Spotify playback…".to_string(),
    }
}

impl Handler {
    pub(super) async fn dispatch_interaction(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Component(component) = &interaction {
            let custom_id = component.data.custom_id.as_str();
            tracing::debug!(custom_id, "button interaction received");

            // The clear confirmation is gated in its own right: a user can
            // leave voice between raising the prompt and pressing the button.
            let allowed = if button_drives_playback(custom_id) {
                self.user_in_bot_voice_channel(&ctx, component.user.id)
            } else if button_mutates_queue(custom_id) {
                self.user_in_any_voice_channel(&ctx, component.user.id)
            } else {
                true
            };
            if !allowed {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(if button_drives_playback(custom_id) {
                            "You must be in the bot's voice channel to control playback."
                        } else {
                            "You must be in a voice channel to change the queue."
                        })
                        .ephemeral(true),
                );
                let _ = component.create_response(&ctx, response).await;
                return;
            }

            // Buttons that own their response shape, rather than producing a
            // line of text for the ephemeral reply below.
            match custom_id {
                "ctrl_queue_hint" => {
                    let content = self.format_queue_listing().await;
                    respond_ephemeral(&ctx, component, content).await;
                    return;
                }
                // Both halves of the clear prompt replace the prompt itself
                // and drop its buttons, so it can't be answered twice.
                "ctrl_queue_clear_confirm" => {
                    let text = self.player.clear_queue().await;
                    replace_prompt(&ctx, component, text).await;
                    return;
                }
                "ctrl_queue_clear_cancel" => {
                    replace_prompt(
                        &ctx,
                        component,
                        "Cancelled — the queue is untouched.".to_string(),
                    )
                    .await;
                    return;
                }
                _ => {}
            }

            let reply_content: String = match custom_id {
                "ctrl_prev" => self.player.previous().await,
                // Same semantics as /skip: the actor cancels the current
                // media item or advances whatever the queue head says.
                "ctrl_next" => self.player.skip().await,
                // ⏯: the actor pauses/resumes the active media item, pauses
                // a playing baseline, or starts/resumes whatever is next.
                "ctrl_pause_toggle" => self.player.toggle_pause().await,
                _ => "Unknown button".to_string(),
            };

            // Ephemeral reply: only the clicker sees the outcome, no channel spam.
            respond_ephemeral(&ctx, component, reply_content).await;
            return;
        }

        let cmd = match interaction.command() {
            Some(c) => c,
            None => { tracing::warn!("interaction was not a command or component"); return; }
        };
        tracing::debug!(command = %cmd.data.name, "processing slash command");

        let user_id = cmd.user.id.to_string();
        let user_id_u64 = cmd.user.id.get();
        let username = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let in_voice = self.user_in_bot_voice_channel(&ctx, cmd.user.id);

        // Handle /play separately (deferred response)
        if cmd.data.name.as_str() == "play" {
            if !self.user_can_play(&ctx, cmd.user.id) {
                let _ = cmd.create_response(&ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("Join a voice channel first (or the bot's channel if it's already in one) to queue playback.")
                        .ephemeral(true),
                )).await;
                return;
            }
            self.handle_play(&cmd, &ctx).await;
            return;
        }

        // Handle /queue separately too: an "Other" (YT/SC/attachment) link
        // spawns the same yt-dlp metadata probe /play does, so it needs the
        // same deferred-response treatment.
        if cmd.data.name.as_str() == "queue" {
            if !in_voice {
                let _ = cmd.create_response(&ctx, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("You must be in the bot's voice channel to control playback.")
                        .ephemeral(true),
                )).await;
                return;
            }
            self.handle_queue(&cmd, &ctx).await;
            return;
        }

        // Defer login immediately — OAuth + session startup takes >3s
        if cmd.data.name.as_str() == "login" {
            let _ = cmd.defer_ephemeral(&ctx).await;
            match self.handle_login(&user_id, user_id_u64, &username, in_voice).await {
                LoginOutcome::Reply(s) => {
                    let _ = cmd.edit_response(&ctx, serenity::builder::EditInteractionResponse::new().content(s)).await;
                }
                LoginOutcome::Pair(auth) => {
                    let _ = cmd.edit_response(
                        &ctx,
                        serenity::builder::EditInteractionResponse::new().content(format!(
                            "Go to <{}> and enter code **{}**.\nThis code expires in 10 minutes.",
                            auth.url(),
                            auth.user_code
                        )),
                    ).await;
                    // Serenity dispatches each interaction in its own task, so
                    // this long await (up to DEVICE_LOGIN_MAX_WAIT) doesn't
                    // block other events.
                    let reply = self.finish_device_login(&user_id, user_id_u64, &username, &ctx, auth).await;
                    let _ = cmd.edit_response(&ctx, serenity::builder::EditInteractionResponse::new().content(reply)).await;
                }
            }
            return;
        }

        let name = cmd.data.name.as_str();
        let allowed = if command_drives_playback(name) {
            in_voice
        } else if command_mutates_queue(name) {
            self.user_in_any_voice_channel(&ctx, cmd.user.id)
        } else {
            true
        };
        if !allowed {
            let _ = cmd.create_response(&ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(if command_drives_playback(name) {
                        "You must be in the bot's voice channel to control playback."
                    } else {
                        "You must be in a voice channel to change the queue."
                    })
                    .ephemeral(true),
            )).await;
            return;
        }

        // /clear asks before it acts: the prompt is ephemeral, so only the
        // person who raised it can answer, and the buttons carry the action.
        if cmd.data.name == "clear" {
            let queued = self.player.query().await.queue_len;
            let response = if queued == 0 {
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("The queue is already empty.")
                        .ephemeral(true),
                )
            } else {
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(format!(
                            "Clear **{queued}** queued track(s)? What's playing now keeps playing."
                        ))
                        .components(vec![clear_confirm_buttons()])
                        .ephemeral(true),
                )
            };
            if let Err(e) = cmd.create_response(&ctx, response).await {
                tracing::warn!(error = ?e, "failed to send the clear confirmation");
            }
            return;
        }

        let reply = match cmd.data.name.as_str() {
            "login" => unreachable!(),
            "logout" => self.handle_logout(&user_id, user_id_u64).await,
            "forget" => self.handle_forget(&user_id, user_id_u64).await,
            "who" => self.handle_who().await,
            "skip" => self.player.skip().await,
            "stop" => self.player.stop().await,
            "np" => render_now_playing(&self.player.query().await.now),
            "history" => {
                let count = cmd
                    .data
                    .options
                    .iter()
                    .find(|o| o.name == "count")
                    .and_then(|o| o.value.as_i64())
                    .unwrap_or(10)
                    .clamp(1, 25) as usize;
                self.handle_history(count).await
            }
            "announce" => self.handle_announce().await,
            _ => return,
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(reply).ephemeral(true),
        );

        if let Err(e) = cmd.create_response(&ctx, response).await {
            tracing::warn!(error = ?e, "failed to create interaction response");
        }
    }

    /// The bot's and the given user's current voice channels, from the cache.
    fn voice_channels(&self, ctx: &Context, user_id: UserId) -> (Option<ChannelId>, Option<ChannelId>) {
        let bot_id = ctx.cache.current_user().id;
        match self.guild_id.to_guild_cached(ctx) {
            Some(guild) => (
                guild.voice_states.get(&bot_id).and_then(|vs| vs.channel_id),
                guild.voice_states.get(&user_id).and_then(|vs| vs.channel_id),
            ),
            None => (None, None),
        }
    }

    /// nob's control rule: a member may drive playback only while sharing the
    /// bot's voice channel. False when the bot isn't in a channel, the user
    /// isn't in one, or they differ.
    pub(super) fn user_in_bot_voice_channel(&self, ctx: &Context, user_id: UserId) -> bool {
        let (bot_ch, user_ch) = self.voice_channels(ctx, user_id);
        voice_gate(bot_ch, user_ch, false)
    }

    /// The looser gate: share the bot's channel when it is in one, otherwise
    /// just be in a voice channel. Used by the queue-only actions, which make
    /// no sound and must stay reachable after a `/stop` has left the channel.
    fn user_in_any_voice_channel(&self, ctx: &Context, user_id: UserId) -> bool {
        let (bot_ch, user_ch) = self.voice_channels(ctx, user_id);
        voice_gate(bot_ch, user_ch, true)
    }

    /// The queue listing shown by the `ctrl_queue_hint` button and by
    /// `/queue` with no arguments, rendered from the actor's
    /// `PlayerSnapshot`: how to add tracks, what's audible right now, and
    /// the first few queued items (with a `+N more` line for the rest).
    async fn format_queue_listing(&self) -> String {
        let snap = self.player.query().await;
        let mut lines = vec![];
        if snap.link_up {
            lines.push("Use `/queue <spotify_url>` to add Spotify tracks.".to_string());
        }
        if self.ytdlp_available {
            lines.push("Use `/play <youtube_url>` to add YouTube tracks.".to_string());
        }
        lines.push(render_now_playing(&snap.now));
        if snap.queue_len > 0 {
            lines.push(format!("\nQueue ({} item(s)):", snap.queue_len));
            for (i, entry) in snap.preview.iter().enumerate() {
                let duration = entry
                    .duration
                    .as_ref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                let armed = if entry.armed { " ⏭ next on Spotify" } else { "" };
                lines.push(format!(
                    "  {}. **{}** — {}{} · queued by {}{}",
                    i + 1,
                    entry.title,
                    entry.subtitle,
                    duration,
                    entry.queued_by,
                    armed
                ));
            }
            if snap.more > 0 {
                lines.push(format!("  +{} more", snap.more));
            }
        }
        lines.join("\n")
    }

    /// Whether a user may queue via /play: if the bot is already in a channel,
    /// they must share it (the control rule); if the bot is in no channel yet,
    /// they only need to be in one so the bot can follow them in.
    fn user_can_play(&self, ctx: &Context, user_id: UserId) -> bool {
        let (bot_ch, user_ch) = self.voice_channels(ctx, user_id);
        voice_gate(bot_ch, user_ch, true)
    }

    /// Extracts `url`/`file`/`next` from a `/play` or `/queue` interaction's
    /// options. `next` is always `false` for commands without that option
    /// (only `/play` registers it).
    fn parse_play_queue_options(
        cmd: &serenity::model::application::CommandInteraction,
    ) -> (Option<String>, Option<serenity::model::channel::Attachment>, bool) {
        let url_arg: Option<String> = cmd.data.options.iter()
            .find(|o| o.name == "url")
            .and_then(|o| if let serenity::model::application::CommandDataOptionValue::String(s) = &o.value { Some(s.clone()) } else { None });
        let attachment_arg = cmd.data.resolved.attachments.values().next().cloned();
        let next: bool = cmd.data.options.iter()
            .find(|o| o.name == "next")
            .and_then(|o| if let serenity::model::application::CommandDataOptionValue::Boolean(b) = &o.value { Some(*b) } else { None })
            .unwrap_or(false);
        (url_arg, attachment_arg, next)
    }

    /// Builds a `QueueItem` from a YouTube/SoundCloud URL (via yt-dlp
    /// metadata) or a file attachment (via extension/size validation). Not
    /// used for Spotify links, whose metadata resolves through the live
    /// session (`PlayerHandle::lookup_spotify`) instead.
    async fn build_media_queue_item(
        url: Option<String>,
        attachment: Option<serenity::model::channel::Attachment>,
        discord_name: &str,
        discord_id: u64,
    ) -> Result<QueueItem, String> {
        if let Some(url) = url {
            let meta = fetch_youtube_metadata(&url).await.map_err(|e| e.to_string())?;
            Ok(QueueItem {
            item_id: 0,
                source: MediaSource::YouTube {
                    url: meta.webpage_url.clone(),
                    video_id: meta.video_id,
                    title: meta.title,
                    channel: meta.channel,
                    thumbnail_url: meta.thumbnail_url,
                    duration_secs: meta.duration_secs,
                },
                queued_by: discord_name.to_string(),
                queued_by_id: discord_id,
            })
        } else {
            let att = attachment.expect("caller ensures url xor attachment is Some");
            validate_attachment(&att.filename, att.size as u64).map_err(|e| e.to_string())?;
            Ok(QueueItem {
            item_id: 0,
                source: MediaSource::File {
                    filename: att.filename.clone(),
                    attachment_url: att.url.clone(),
                },
                queued_by: discord_name.to_string(),
                queued_by_id: discord_id,
            })
        }
    }

    async fn handle_play(
        &self,
        cmd: &serenity::model::application::CommandInteraction,
        ctx: &Context,
    ) {
        let (url_arg, attachment_arg, next) = Self::parse_play_queue_options(cmd);

        if url_arg.is_none() && attachment_arg.is_none() {
            // Bare `/play` is ▶: start whatever is up when nothing is
            // audible. It never pauses — with something playing it asks
            // for a link, so a fat-fingered `/play` can't cut the music.
            let text = self.player.play().await;
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().content(text).ephemeral(true)
            )).await;
            return;
        }
        if url_arg.is_some() && attachment_arg.is_some() {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ Provide either a URL or a file, not both.")
                    .ephemeral(true)
            )).await;
            return;
        }

        let discord_name = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let discord_id = cmd.user.id.get();

        // Spotify track link: no yt-dlp probe, but `ensure_session` below
        // can wait up to 15s for a session to come up, which blows
        // Discord's 3s window — defer first, exactly like the media path
        // below. Metadata resolves here in the handler task (never inside
        // the actor); the actor then owns the enqueue-and-maybe-start
        // decision and the reply.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
                    CreateInteractionResponseMessage::new().ephemeral(true)
                )).await;

                let reply = match self.supervisor.ensure_session(&self.oauth, &self.user_store).await {
                    EnsureOutcome::NoAccount => {
                        "No Spotify account is connected — someone needs to run `/login`.".to_string()
                    }
                    EnsureOutcome::Failed(reason) => {
                        tracing::warn!(error = %reason, "ensure_session failed for /play");
                        "⚠️ Couldn't reach Spotify — try again in a moment.".to_string()
                    }
                    EnsureOutcome::Ready(_gen) => match self.player.lookup_spotify(&spotify_uri).await {
                        None => "⚠️ Couldn't find that Spotify track — check the link.".to_string(),
                        Some((title, artist, album_art_url)) => {
                            let item = QueueItem {
                                item_id: 0,
                                source: MediaSource::Spotify { uri: spotify_uri, title, artist, album_art_url },
                                queued_by: discord_name.clone(),
                                queued_by_id: discord_id,
                            };
                            let pos = if next {
                                // An armed head is already on Spotify's own
                                // device queue and can't be un-queued — a
                                // "next" item lands right behind it instead
                                // of jumping it, so the listing matches the
                                // air order.
                                let head_armed = self
                                    .player
                                    .query()
                                    .await
                                    .preview
                                    .first()
                                    .is_some_and(|entry| entry.armed);
                                if head_armed { EnqueuePos::At(1) } else { EnqueuePos::Head }
                            } else {
                                EnqueuePos::Tail
                            };
                            self.player.enqueue(item, pos, true).await
                        }
                    },
                };
                let _ = cmd.edit_response(ctx, EditInteractionResponse::new().content(reply)).await;
                return;
            }
        }

        if !self.ytdlp_available {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ YouTube playback is not available (yt-dlp not installed).")
                    .ephemeral(true)
            )).await;
            return;
        }

        // Per-user cooldown ahead of the metadata probe: every /play spawns a
        // yt-dlp subprocess before the queue cap applies, so rapid calls
        // would otherwise drive unbounded process pressure.
        const PLAY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);
        let on_cooldown = {
            let now = Instant::now();
            let mut lock = self.play_cooldowns.lock();
            match lock.get(&cmd.user.id.get()) {
                Some(last) if now.duration_since(*last) < PLAY_COOLDOWN => true,
                _ => {
                    lock.insert(cmd.user.id.get(), now);
                    false
                }
            }
        };
        if on_cooldown {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⏳ One /play at a time — try again in a few seconds.")
                    .ephemeral(true)
            )).await;
            return;
        }

        // Defer response
        let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true)
        )).await;

        let queue_item = match Self::build_media_queue_item(url_arg, attachment_arg, &discord_name, discord_id).await {
            Ok(item) => item,
            Err(e) => {
                let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
                    .content(format!("❌ {}", e))
                ).await;
                return;
            }
        };

        // The actor owns the enqueue-and-maybe-start decision from here: it
        // pushes into its owned queue, starts the head when nothing holds
        // the turn, and formats the reply.
        let reply = self
            .player
            .enqueue(
                queue_item,
                if next { EnqueuePos::Head } else { EnqueuePos::Tail },
                true,
            )
            .await;

        let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
            .content(reply)
        ).await;
    }

    /// The last `count` tracks that actually aired, newest first. Requests
    /// name whoever asked for them; the DJ's own playlist tracks don't, so
    /// the two are told apart at a glance.
    async fn handle_history(&self, count: usize) -> String {
        let Some(store) = self.history.clone() else {
            return "⚠️ No play history is being kept — the database couldn't be opened."
                .to_string();
        };
        // SQLite is blocking; keep it off the interaction task's thread.
        let rows = tokio::task::spawn_blocking(move || store.recent(count)).await;
        let rows = match rows {
            Ok(Ok(rows)) => rows,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to read the play history");
                return "⚠️ Couldn't read the play history.".to_string();
            }
            Err(e) => {
                tracing::warn!(error = %e, "play history read panicked");
                return "⚠️ Couldn't read the play history.".to_string();
            }
        };
        render_history(&rows)
    }

    async fn handle_announce(&self) -> String {
        let current = self.announce_enabled.load(Ordering::Relaxed);
        let new_val = !current;
        self.announce_enabled.store(new_val, Ordering::Relaxed);
        // Persist so restarts (including the VPS updater's) keep the toggle.
        if let Err(e) = self.user_store.set_setting("announce_enabled", if new_val { "1" } else { "0" }) {
            tracing::warn!(error = %e, "failed to persist announce toggle");
        }
        if new_val {
            "🎙️ DJ track announcements **enabled**. Spotibot will announce each track.".to_string()
        } else {
            "🔇 DJ track announcements **disabled**. Greetings still active.".to_string()
        }
    }

    async fn handle_who(&self) -> String {
        let lock = self.active_session.lock();
        match lock.as_ref() {
            // One name: the Web API profile lookup this used to pair with
            // Discord's own name is gone (429s under the desktop client id),
            // so there is only ever the one name to show.
            Some(session) => format!("Active session: **{}**", session.discord_name),
            None => "No Spotify session — run `/login` to connect.".to_string(),
        }
    }

    /// `/queue`: adds to the queue's tail without starting playback —
    /// Spotify, YouTube/SoundCloud and attachments all land in the actor's
    /// one unified queue (docs/PORT.md decision #15), never jump the line, never
    /// start playback. No arguments shows the current queue listing.
    async fn handle_queue(
        &self,
        cmd: &serenity::model::application::CommandInteraction,
        ctx: &Context,
    ) {
        let (url_arg, attachment_arg, _next) = Self::parse_play_queue_options(cmd);

        if url_arg.is_none() && attachment_arg.is_none() {
            let content = self.format_queue_listing().await;
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().content(content).ephemeral(true)
            )).await;
            return;
        }
        if url_arg.is_some() && attachment_arg.is_some() {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ Provide either a URL or a file, not both.")
                    .ephemeral(true)
            )).await;
            return;
        }

        let discord_name = cmd.user.global_name.clone().unwrap_or_else(|| cmd.user.name.clone());
        let discord_id = cmd.user.id.get();

        // Spotify track link: no yt-dlp probe, but `ensure_session` below
        // can wait up to 15s for a session to come up, which blows
        // Discord's 3s window — defer first, exactly like the media path
        // below. Metadata resolves here in the handler task (never inside
        // the actor); the actor owns the tail push and the reply, with
        // `start_if_idle` off.
        if let Some(url) = &url_arg {
            if let LinkKind::Spotify(spotify_uri) = classify_link(url) {
                let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
                    CreateInteractionResponseMessage::new().ephemeral(true)
                )).await;

                let reply = match self.supervisor.ensure_session(&self.oauth, &self.user_store).await {
                    EnsureOutcome::NoAccount => {
                        "No Spotify account is connected — someone needs to run `/login`.".to_string()
                    }
                    EnsureOutcome::Failed(reason) => {
                        tracing::warn!(error = %reason, "ensure_session failed for /queue");
                        "⚠️ Couldn't reach Spotify — try again in a moment.".to_string()
                    }
                    EnsureOutcome::Ready(_gen) => match self.player.lookup_spotify(&spotify_uri).await {
                        None => "⚠️ Couldn't find that Spotify track — check the link.".to_string(),
                        Some((title, artist, album_art_url)) => {
                            let item = QueueItem {
                                item_id: 0,
                                source: MediaSource::Spotify { uri: spotify_uri, title, artist, album_art_url },
                                queued_by: discord_name.clone(),
                                queued_by_id: discord_id,
                            };
                            self.player.enqueue(item, EnqueuePos::Tail, false).await
                        }
                    },
                };
                let _ = cmd.edit_response(ctx, EditInteractionResponse::new().content(reply)).await;
                return;
            }
        }

        // YouTube/SoundCloud URL, or a file attachment: goes on the queue's
        // tail via the same metadata probe /play uses.
        if url_arg.is_some() && !self.ytdlp_available {
            let _ = cmd.create_response(ctx, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ YouTube playback is not available (yt-dlp not installed).")
                    .ephemeral(true)
            )).await;
            return;
        }

        let _ = cmd.create_response(ctx, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true)
        )).await;

        let queue_item = match Self::build_media_queue_item(url_arg, attachment_arg, &discord_name, discord_id).await {
            Ok(item) => item,
            Err(e) => {
                let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
                    .content(format!("❌ {}", e))
                ).await;
                return;
            }
        };

        // `/queue` never starts playback: the actor pushes to the tail and
        // formats the reply, with `start_if_idle` off.
        let reply = self.player.enqueue(queue_item, EnqueuePos::Tail, false).await;

        let _ = cmd.edit_response(ctx, EditInteractionResponse::new()
            .content(reply)
        ).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        button_drives_playback, button_mutates_queue, command_drives_playback,
        command_mutates_queue, is_valid_track_id, parse_track_id_from_url, render_history,
        render_now_playing, voice_gate, NowPlaying,
    };
    use crate::history::HistoryRow;
    use crate::player::state::AiredSource;
    use serenity::all::ChannelId;

    const ID: &str = "4cOdK2wGLETKBW3PvgPWqT"; // 22 base62 chars

    fn row(title: &str, who: Option<&str>) -> HistoryRow {
        HistoryRow {
            id: 1,
            aired_at_unix: 1_788_000_000,
            source: AiredSource::Baseline,
            track_ref: "spotify:track:x".into(),
            context_uri: None,
            title: Some(title.into()),
            artist: Some("An Artist".into()),
            queued_by: who.map(String::from),
        }
    }

    #[test]
    fn history_stamps_each_row_with_a_timestamp_discord_will_expand() {
        // Bare markup, not code-fenced: in backticks Discord shows the raw
        // "<t:...>" instead of a time, which is the whole point of storing it.
        let out = render_history(&[row("A Track", None)]);
        assert!(out.contains("<t:1788000000:t>"), "{out}");
        assert!(!out.contains("`<t:"), "a fenced timestamp renders literally: {out}");
    }

    #[test]
    fn history_names_the_requester_only_for_requests() {
        let out = render_history(&[row("Asked For", Some("Papos")), row("Just Played", None)]);
        assert!(out.contains("**Asked For** — An Artist (queued by Papos)"), "{out}");
        assert!(out.contains("**Just Played** — An Artist\n"), "{out}");
        assert!(!out.contains("Just Played** — An Artist (queued"), "{out}");
    }

    #[test]
    fn history_falls_back_when_metadata_is_missing() {
        let mut r = row("x", None);
        r.title = None;
        r.artist = None;
        let out = render_history(&[r]);
        assert!(out.contains("**Unknown track** — Unknown artist"), "{out}");
    }

    #[test]
    fn an_empty_history_says_so_rather_than_showing_a_heading() {
        assert_eq!(render_history(&[]), "Nothing has played yet.");
    }

    #[test]
    fn a_long_history_stays_inside_discords_message_limit() {
        // 25 rows of realistic Spotify titles blow past 2000 characters, and
        // Discord rejects the whole message rather than truncating it.
        let long = "A Very Long Remastered Track Title (2011 Remaster) - Extended";
        let rows: Vec<_> = (0..25).map(|_| row(long, Some("SomebodyWithALongName"))).collect();
        let out = render_history(&rows);
        assert!(out.len() <= 2000, "would be rejected outright: {} chars", out.len());
        assert!(out.contains("…and "), "and it says what it left out: {out}");
    }

    // --- voice_gate: the authorization rule behind every playback command ---

    #[test]
    fn gate_requires_sharing_the_bots_channel() {
        let bot = Some(ChannelId::new(10));
        assert!(voice_gate(bot, Some(ChannelId::new(10)), false), "same channel passes");
        assert!(!voice_gate(bot, Some(ChannelId::new(11)), false), "other channel fails");
        assert!(!voice_gate(bot, None, false), "not in voice fails");
        // allow_follow changes nothing while the bot IS in a channel.
        assert!(!voice_gate(bot, Some(ChannelId::new(11)), true));
        assert!(!voice_gate(bot, None, true));
    }

    #[test]
    fn gate_with_bot_out_of_voice_depends_on_follow() {
        // Strict commands (buttons, /skip, /stop…) fail when the bot isn't in
        // voice; /play's follow mode only needs the requester to be in one.
        assert!(!voice_gate(None, Some(ChannelId::new(10)), false));
        assert!(voice_gate(None, Some(ChannelId::new(10)), true));
        assert!(!voice_gate(None, None, true), "follow still needs the user in voice");
    }

    // --- which interactions each gate covers -----------------------------

    #[test]
    fn only_the_audible_commands_take_the_strict_gate() {
        assert!(command_drives_playback("skip"));
        assert!(command_drives_playback("stop"));
        // Reads and settings stay open, and /play runs its own follow gate
        // before this point.
        for open in ["np", "queue", "history", "who", "announce", "login", "play"] {
            assert!(!command_drives_playback(open), "{open} must not be gated on the bot's channel");
        }
    }

    #[test]
    fn clear_takes_the_looser_gate_because_stop_points_at_it() {
        // `/stop` leaves the channel and its reply says the queue survived
        // and to use `/clear`. Under the strict gate that command would be
        // refused in exactly the state the message describes.
        assert!(command_mutates_queue("clear"));
        assert!(!command_drives_playback("clear"), "never both");
        for other in ["skip", "stop", "np", "queue"] {
            assert!(!command_mutates_queue(other));
        }
    }

    #[test]
    fn the_buttons_split_the_same_way_as_the_commands() {
        for playback in ["ctrl_prev", "ctrl_next", "ctrl_pause_toggle"] {
            assert!(button_drives_playback(playback));
            assert!(!button_mutates_queue(playback), "never both");
        }
        assert!(button_mutates_queue("ctrl_queue_clear_confirm"));
        assert!(!button_drives_playback("ctrl_queue_clear_confirm"));
        // Reading the queue and backing out of the prompt are open.
        for open in ["ctrl_queue_hint", "ctrl_queue_clear_cancel"] {
            assert!(!button_drives_playback(open) && !button_mutates_queue(open), "{open}");
        }
    }

    // --- render_now_playing: the /np and queue-listing status line ---

    #[test]
    fn renders_nothing_with_the_house_phrasing() {
        assert_eq!(render_now_playing(&NowPlaying::Nothing), "Nothing is playing right now.");
    }

    #[test]
    fn renders_a_media_item_with_requester_and_pause_glyph() {
        let now = NowPlaying::Media {
            title: "Song".into(),
            subtitle: "Channel".into(),
            queued_by: "DJ".into(),
            paused: false,
        };
        assert_eq!(render_now_playing(&now), "▶ **Song** — Channel · queued by **DJ**");
        let paused = NowPlaying::Media {
            title: "Song".into(),
            subtitle: "Channel".into(),
            queued_by: "DJ".into(),
            paused: true,
        };
        assert!(render_now_playing(&paused).starts_with('⏸'));
    }

    #[test]
    fn renders_a_spotify_track_with_pause_glyph() {
        let now = NowPlaying::Spotify { title: "Song".into(), artist: "Artist".into(), paused: false };
        assert_eq!(render_now_playing(&now), "▶ **Song** — Artist");
        let paused = NowPlaying::Spotify { title: "Song".into(), artist: "Artist".into(), paused: true };
        assert!(render_now_playing(&paused).starts_with('⏸'));
    }

    #[test]
    fn renders_a_pending_spotify_start() {
        assert!(render_now_playing(&NowPlaying::SpotifyStarting).starts_with('▶'));
    }

    #[test]
    fn parses_plain_url() {
        assert_eq!(
            parse_track_id_from_url(&format!("https://open.spotify.com/track/{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn parses_url_with_si_query() {
        assert_eq!(
            parse_track_id_from_url(&format!("https://open.spotify.com/track/{ID}?si=abc123")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn parses_locale_prefixed_url() {
        assert_eq!(
            parse_track_id_from_url(&format!("https://open.spotify.com/intl-fr/track/{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn parses_uri() {
        assert_eq!(
            parse_track_id_from_url(&format!("spotify:track:{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn rejects_query_param_injection() {
        // A crafted id with an extra param must not survive validation, or it
        // would ride into the authenticated queue POST's query string.
        assert!(parse_track_id_from_url(&format!("spotify:track:{ID}&device_id=x")).is_none());
        assert!(parse_track_id_from_url("spotify:track:abc&foo=bar").is_none());
    }

    #[test]
    fn rejects_wrong_length_and_nonalnum() {
        assert!(!is_valid_track_id("too-short"));
        assert!(!is_valid_track_id(&"x".repeat(23)));
        assert!(!is_valid_track_id("4cOdK2wGLETKBW3PvgPWq!")); // 22 chars, bad byte
        assert!(is_valid_track_id(ID));
    }

    #[test]
    fn rejects_unrelated_input() {
        assert!(parse_track_id_from_url("https://youtube.com/watch?v=abc").is_none());
        assert!(parse_track_id_from_url("just some text").is_none());
    }
}
