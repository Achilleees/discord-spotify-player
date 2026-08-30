//! Single owner of the now-playing/controls card.
//!
//! Every post/edit/delete against the channel's now-playing card goes
//! through one task and one mailbox (`UiMsg`), keyed on one `card_id` —
//! both the Spotify baseline and the priority (YouTube/file) queue send
//! here rather than touching the channel directly, so the two playback
//! sources can never race each other's post/delete.

use super::bot::SpotifyTrackInfo;
use crate::queue::{MediaSource, QueueItem};
use serenity::all::ChannelId;
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateMessage, EditMessage};
use serenity::client::Context;
use serenity::model::application::ButtonStyle;
use serenity::model::id::MessageId;
use serenity::model::Timestamp;
use tokio::sync::mpsc;

/// What to render on the now-playing card.
#[derive(Debug, Clone)]
pub enum CardView {
    Spotify {
        title: String,
        artist: String,
        track_id: String,
        album_art_url: Option<String>,
        dj_name: String,
    },
    Queued { item: QueueItem },
}

/// What to render on a history embed — the same two shapes as `CardView`,
/// for a track/item that has just been superseded.
#[derive(Debug, Clone)]
/// `Spotify` is posted automatically from the previous card; the explicit
/// variant is wired in C5 when the actor owns card triggering.
#[allow(dead_code)]
pub enum HistoryView {
    Spotify {
        title: String,
        artist: String,
        track_id: String,
        album_art_url: Option<String>,
        dj_name: String,
    },
    Queued { item: QueueItem },
}

/// Messages the UI task accepts.
pub enum UiMsg {
    /// A new track/item started. Posts history for whatever this
    /// supersedes when both are Spotify tracks (a Queued predecessor's
    /// history always arrives via an explicit `History` message instead,
    /// since a queue item's completion doesn't necessarily coincide with
    /// whatever plays next); deletes the old card; posts the new one.
    NowPlaying(CardView),
    /// Post a history embed immediately, independent of the current card.
    History(HistoryView),
    /// Nothing is playing: delete the current card and post the idle
    /// controls card (with buttons only when `account` is `Some`).
    Idle { account: Option<String> },
    /// Edit the current card's buttons in place (pause/resume glyph).
    Buttons { paused: bool },
    /// Update the displayed account name without a track interruption.
    /// Currently equivalent to `Idle`; kept distinct so a future card
    /// layout can special-case an in-place name update instead of a full
    /// repost.
    /// Producer arrives in C4, when the session supervisor owns account switches.
    #[allow(dead_code)]
    AccountChanged(Option<String>),
}

/// Spawns the UI task and returns its mailbox. Call exactly once, from
/// `ready()`, behind the same one-shot guard the presence loop's own spawn
/// uses — a second call spawns a second task racing the first over the
/// same card.
pub fn spawn(ctx: Context, text_channel_id: ChannelId) -> mpsc::UnboundedSender<UiMsg> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(ctx, text_channel_id, rx));
    tx
}

async fn run(ctx: Context, text_channel_id: ChannelId, mut rx: mpsc::UnboundedReceiver<UiMsg>) {
    startup_sweep(&ctx, text_channel_id).await;
    let mut card_id = post_controls(&ctx, text_channel_id, None).await;
    let mut prev_card: Option<CardView> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            UiMsg::NowPlaying(view) => {
                if let (Some(CardView::Spotify { title, artist, track_id, album_art_url, dj_name }), CardView::Spotify { .. }) =
                    (&prev_card, &view)
                {
                    let meta = SpotifyTrackInfo {
                        title: title.clone(),
                        artist: artist.clone(),
                        album_art_url: album_art_url.clone(),
                    };
                    let history_embed = build_history_embed(&meta, track_id, dj_name);
                    let msg = CreateMessage::new().embed(history_embed);
                    let _ = text_channel_id.send_message(&ctx, msg).await;
                }

                if let Some(mid) = card_id.take() {
                    let _ = text_channel_id.delete_message(&ctx, mid).await;
                }

                let embed = match &view {
                    CardView::Spotify { title, artist, track_id, album_art_url, dj_name } => {
                        let meta = SpotifyTrackInfo {
                            title: title.clone(),
                            artist: artist.clone(),
                            album_art_url: album_art_url.clone(),
                        };
                        build_now_playing_embed(&meta, track_id, dj_name)
                    }
                    CardView::Queued { item } => build_priority_now_playing_embed(item),
                };
                let buttons = build_controls_buttons(false);
                let post = CreateMessage::new().embed(embed).components(vec![buttons]);
                match text_channel_id.send_message(&ctx, post).await {
                    Ok(m) => {
                        if let CardView::Spotify { title, artist, .. } = &view {
                            println!("Playing: {} - {}", title, artist);
                            tracing::info!(title = %title, artist = %artist, "now-playing embed sent");
                        }
                        card_id = Some(m.id);
                    }
                    Err(e) => tracing::warn!(error = ?e, "failed to send now-playing embed"),
                }

                prev_card = Some(view);
            }
            UiMsg::History(view) => {
                let embed = match view {
                    HistoryView::Spotify { title, artist, track_id, album_art_url, dj_name } => {
                        let meta = SpotifyTrackInfo { title, artist, album_art_url };
                        build_history_embed(&meta, &track_id, &dj_name)
                    }
                    HistoryView::Queued { item } => build_priority_history_embed(&item),
                };
                let msg = CreateMessage::new().embed(embed);
                let _ = text_channel_id.send_message(&ctx, msg).await;
            }
            UiMsg::Idle { account } => {
                if let Some(mid) = card_id.take() {
                    let _ = text_channel_id.delete_message(&ctx, mid).await;
                }
                card_id = post_controls(&ctx, text_channel_id, account.as_deref()).await;
                prev_card = None;
            }
            UiMsg::AccountChanged(account) => {
                if let Some(mid) = card_id.take() {
                    let _ = text_channel_id.delete_message(&ctx, mid).await;
                }
                card_id = post_controls(&ctx, text_channel_id, account.as_deref()).await;
                prev_card = None;
            }
            UiMsg::Buttons { paused } => {
                if let Some(mid) = card_id {
                    let buttons = build_controls_buttons(paused);
                    let edit = EditMessage::new().components(vec![buttons]);
                    let _ = text_channel_id.edit_message(&ctx, mid, edit).await;
                }
            }
        }
    }
}

/// Deletes any of the bot's own leftover control/now-playing messages
/// (identified by author plus buttons or a "🎛️" embed title, since a
/// restart has no other way to recognize its own old cards) before the
/// task starts serving `UiMsg`s.
async fn startup_sweep(ctx: &Context, text_channel_id: ChannelId) {
    use serenity::all::GetMessages;
    let bot_id = ctx.cache.current_user().id;
    let builder = GetMessages::new().limit(20);
    if let Ok(messages) = text_channel_id.messages(ctx, builder).await {
        for msg in &messages {
            if msg.author.id != bot_id {
                continue;
            }
            // A stale control/now-playing message is any of ours that still
            // carries buttons, or whose embed is one of our control cards
            // (idle "🎛️ Spotibot" or an active "🎛️ {name}"). Matching on the
            // buttons catches the merged now-playing card too, whose title is
            // the track name rather than a "🎛️" string.
            let has_buttons = !msg.components.is_empty();
            let is_control_card = msg
                .embeds
                .iter()
                .any(|e| e.title.as_deref().is_some_and(|t| t.starts_with("🎛️")));
            if has_buttons || is_control_card {
                let _ = text_channel_id.delete_message(ctx, msg.id).await;
            }
        }
    }
}

async fn post_controls(ctx: &Context, text_channel_id: ChannelId, active_user: Option<&str>) -> Option<MessageId> {
    let embed = build_controls_embed(active_user);
    let mut msg = CreateMessage::new().embed(embed);
    if active_user.is_some() {
        msg = msg.components(vec![build_controls_buttons(false)]);
    }
    match text_channel_id.send_message(ctx, msg).await {
        Ok(m) => {
            tracing::info!("posted controls message");
            Some(m.id)
        }
        Err(e) => {
            tracing::warn!(error = ?e, "failed to post controls message");
            None
        }
    }
}

// --- Embed builders ---

fn build_now_playing_embed(meta: &SpotifyTrackInfo, track_id: &str, spotify_name: &str) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(0x1DB954u32)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(format!("{} — {}", meta.title, meta.artist))
        .url(format!("https://open.spotify.com/track/{}", track_id))
        .timestamp(Timestamp::now());

    if !spotify_name.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(format!("🎧 {}", spotify_name)));
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.image(art_url);
    }

    embed
}

fn build_history_embed(meta: &SpotifyTrackInfo, track_id: &str, spotify_name: &str) -> CreateEmbed {
    let footer_text = if spotify_name.is_empty() {
        String::new()
    } else {
        format!("played by {}", spotify_name)
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(format!(
            "[{} — {}](https://open.spotify.com/track/{})",
            meta.title, meta.artist, track_id
        ));

    if !footer_text.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(footer_text));
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.thumbnail(art_url);
    }

    embed
}

fn build_priority_now_playing_embed(item: &QueueItem) -> CreateEmbed {
    let color = item.source.embed_color();
    let title = item.source.display_title();
    let subtitle = item.source.display_subtitle();
    let footer_icon = match &item.source {
        MediaSource::YouTube { .. } => "🎬",
        MediaSource::File { .. } => "📎",
        MediaSource::Spotify { .. } => "🎵",
    };

    let footer_text = match item.source.display_duration() {
        Some(d) => format!("{} {} · {}", footer_icon, item.queued_by, d),
        None => format!("{} {}", footer_icon, item.queued_by),
    };
    let mut embed = CreateEmbed::new()
        .color(color)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(format!("{} — {}", title, subtitle))
        .timestamp(Timestamp::now())
        .footer(CreateEmbedFooter::new(footer_text));

    if let MediaSource::YouTube { video_id, thumbnail_url, .. } = &item.source {
        let url = format!("https://www.youtube.com/watch?v={}", video_id);
        embed = embed.url(url);
        if let Some(thumb) = thumbnail_url {
            embed = embed.image(thumb);
        }
    } else if let MediaSource::Spotify { album_art_url: Some(art), .. } = &item.source {
        embed = embed.image(art);
    }

    embed
}

fn build_priority_history_embed(item: &QueueItem) -> CreateEmbed {
    let footer_text = match item.source.display_duration() {
        Some(d) => format!("played by {} · {}", item.queued_by, d),
        None => format!("played by {}", item.queued_by),
    };
    let description = match &item.source {
        MediaSource::YouTube { title, channel, video_id, .. } => {
            format!("[{} — {}](https://www.youtube.com/watch?v={})", title, channel, video_id)
        }
        MediaSource::File { filename, .. } => {
            format!("📎 {}", filename)
        }
        MediaSource::Spotify { title, artist, .. } => {
            format!("🎵 {} — {}", title, artist)
        }
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(description)
        .footer(CreateEmbedFooter::new(footer_text));

    if let MediaSource::YouTube { thumbnail_url: Some(thumb), .. } = &item.source {
        embed = embed.thumbnail(thumb);
    } else if let MediaSource::Spotify { album_art_url: Some(art), .. } = &item.source {
        embed = embed.thumbnail(art);
    }

    embed
}

/// The idle controls card. Once a track is playing, the now-playing embed
/// (which carries its own buttons) supersedes this, so there is no separate
/// "is playing" state to render here.
fn build_controls_embed(active_user: Option<&str>) -> CreateEmbed {
    match active_user {
        Some(name) => CreateEmbed::new()
            .color(0x1DB954u32)
            .title(format!("🎛️ {}", name))
            .description("*Play something to get started!*"),
        None => CreateEmbed::new()
            .color(0x5865F2u32)
            .title("🎛️ Spotibot")
            .description("*Use `/login` to start a session*"),
    }
}

fn build_controls_buttons(is_paused: bool) -> CreateActionRow {
    let pause_label = if is_paused { "▶" } else { "⏸" };
    CreateActionRow::Buttons(vec![
        CreateButton::new("ctrl_prev").label("⏮").style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_pause_toggle").label(pause_label).style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_next").label("⏭").style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_queue_hint").label("➕ Queue").style(ButtonStyle::Secondary),
    ])
}
